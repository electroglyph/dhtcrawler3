//! User query syntax and its limits.
//!
//! Syntax: whitespace-separated words; `-word` excludes a word; `"a b"` is a
//! phrase; the last word also matches as a prefix unless the query ends with
//! whitespace. Nothing else is special: `name:foo`, `OR`, `*` and the like are
//! ordinary text and go through the `dc3` tokenizer like any other word.
//!
//! A word that is a single CJK character searches the `*_cjk1` unigram
//! fields; every other word searches the `dc3` word and bigram fields.

use serde::{Deserialize, Serialize};

use crate::tokenizer::{Dc3Tokenizer, contains_cjk, is_cjk_unigram};

/// Maximum query length in characters.
pub const MAX_QUERY_CHARS: usize = 200;
/// Maximum number of words (including excluded ones) in a query.
pub const MAX_TERMS: usize = 12;
/// Maximum index tokens kept from one word. One long CJK word tokenizes to
/// hundreds of overlapping bigrams; without a cap a single word becomes a
/// hundred-term phrase (times two fields). Longer words are truncated.
pub const MAX_TOKENS_PER_WORD: usize = 16;
/// Highest page number that may be requested.
pub const MAX_PAGE: u32 = 50;
/// Largest page size that may be requested.
pub const MAX_PER_PAGE: u32 = 50;
/// Page size the web front end uses.
pub const DEFAULT_PER_PAGE: u32 = 20;
/// A prefix must be at least this many characters long to be expanded.
pub const PREFIX_MIN_CHARS: usize = 2;
/// Maximum number of index terms a prefix expands to, per field.
pub const PREFIX_MAX_EXPANSIONS: usize = 200;

/// Why a query was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("query is {chars} characters long; the limit is {max}")]
    TooLong { chars: usize, max: usize },
    #[error("query has {terms} words; the limit is {max}")]
    TooManyTerms { terms: usize, max: usize },
    #[error("query has no searchable words")]
    NoTerms,
    #[error("page {page} is out of range 1..={max}")]
    PageOutOfRange { page: u32, max: u32 },
    #[error("page size {per_page} is out of range 1..={max}")]
    PerPageOutOfRange { per_page: u32, max: u32 },
}

/// Result ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    /// BM25 weighted by popularity.
    #[default]
    Relevance,
    /// Newest `created` first.
    Newest,
    /// Largest `size` first.
    Size,
    /// Most `seen` first.
    Seen,
}

impl Sort {
    /// Lowercase name, as used in URLs.
    pub fn as_str(self) -> &'static str {
        match self {
            Sort::Relevance => "relevance",
            Sort::Newest => "newest",
            Sort::Size => "size",
            Sort::Seen => "seen",
        }
    }

    /// Parses the lowercase name; unknown names give `None`.
    pub fn parse(s: &str) -> Option<Sort> {
        match s {
            "relevance" => Some(Sort::Relevance),
            "newest" => Some(Sort::Newest),
            "size" => Some(Sort::Size),
            "seen" => Some(Sort::Seen),
            _ => None,
        }
    }
}

/// A search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub sort: Sort,
    /// 1-based page number, at most [`MAX_PAGE`].
    pub page: u32,
    /// Results per page, at most [`MAX_PER_PAGE`].
    pub per_page: u32,
}

impl SearchQuery {
    /// First page of `text` by relevance with [`DEFAULT_PER_PAGE`] results.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            sort: Sort::Relevance,
            page: 1,
            per_page: DEFAULT_PER_PAGE,
        }
    }

    /// Checks page limits and returns the number of results to skip.
    pub fn offset(&self) -> Result<usize, QueryError> {
        if self.page == 0 || self.page > MAX_PAGE {
            return Err(QueryError::PageOutOfRange {
                page: self.page,
                max: MAX_PAGE,
            });
        }
        if self.per_page == 0 || self.per_page > MAX_PER_PAGE {
            return Err(QueryError::PerPageOutOfRange {
                per_page: self.per_page,
                max: MAX_PER_PAGE,
            });
        }
        // Both factors are at most 50, so this cannot overflow.
        let skipped =
            u64::from(self.page.saturating_sub(1)).saturating_mul(u64::from(self.per_page));
        Ok(usize::try_from(skipped).unwrap_or(usize::MAX))
    }
}

/// One word or phrase of a parsed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryWord {
    /// `dc3` tokens of the word; never empty.
    pub tokens: Vec<String>,
    /// The word was written `-word` and must not match.
    pub exclude: bool,
    /// The word was written in double quotes.
    pub quoted: bool,
    /// The last token should also match as a prefix.
    pub prefix: bool,
}

impl QueryWord {
    /// True when the word is a single CJK character, which the `*_cjk1`
    /// fields answer.
    pub fn is_cjk_unigram(&self) -> bool {
        matches!(self.tokens.as_slice(), [only] if is_cjk_unigram(only))
    }
}

/// A validated query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Words in query order.
    pub words: Vec<QueryWord>,
}

impl ParsedQuery {
    /// Words that must match.
    pub fn positive(&self) -> impl Iterator<Item = &QueryWord> {
        self.words.iter().filter(|w| !w.exclude)
    }

    /// Words that must not match.
    pub fn negative(&self) -> impl Iterator<Item = &QueryWord> {
        self.words.iter().filter(|w| w.exclude)
    }
}

struct RawWord {
    text: String,
    exclude: bool,
    quoted: bool,
}

/// Parses and validates user query text. Never matches everything: a query
/// without a positive word is [`QueryError::NoTerms`].
pub fn parse_query(text: &str) -> Result<ParsedQuery, QueryError> {
    let chars = text.chars().count();
    if chars > MAX_QUERY_CHARS {
        return Err(QueryError::TooLong {
            chars,
            max: MAX_QUERY_CHARS,
        });
    }
    let raw = split_words(text);
    let ends_open = text.chars().last().is_some_and(|c| !c.is_whitespace());

    let mut tokenizer = Dc3Tokenizer::new();
    let mut words = Vec::with_capacity(raw.len());
    let mut trailing_ignored = false;
    for w in raw {
        let mut tokens = tokenizer.token_texts(&w.text);
        if tokens.is_empty() {
            trailing_ignored = true;
            continue;
        }
        tokens.truncate(MAX_TOKENS_PER_WORD);
        if words.len() >= MAX_TERMS {
            return Err(QueryError::TooManyTerms {
                terms: words.len().saturating_add(1),
                max: MAX_TERMS,
            });
        }
        words.push(QueryWord {
            tokens,
            exclude: w.exclude,
            quoted: w.quoted,
            prefix: false,
        });
        trailing_ignored = false;
    }

    if !words.iter().any(|w| !w.exclude) {
        return Err(QueryError::NoTerms);
    }

    // The quote that closes a phrase also closes the query for prefix
    // purposes, so only an unquoted trailing word qualifies. Trailing words
    // without tokens (for example `!!!`) are ignorable and must not flip the
    // preceding word to a prefix. The length gate counts the folded tokens,
    // not raw characters, so canonically identical tokens behave alike; the
    // total folded length gates (so multi-token words like `22.0` keep their
    // phrase prefix) while the CJK check stays on the last token that the
    // prefix expands.
    if ends_open && !trailing_ignored && let Some(last) = words.last_mut() {
        let eligible = !last.exclude
            && !last.quoted
            && last.tokens.iter().map(|t| t.chars().count()).sum::<usize>()
                >= PREFIX_MIN_CHARS
            && last.tokens.last().is_some_and(|t| !contains_cjk(t));
        last.prefix = eligible;
    }

    Ok(ParsedQuery { words })
}

fn split_words(text: &str) -> Vec<RawWord> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let exclude = chars.next_if_eq(&'-').is_some();
        let quoted = chars.next_if_eq(&'"').is_some();
        let mut word = String::new();
        if quoted {
            // An unterminated quote runs to the end of the text.
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                word.push(c);
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace() && *c != '"') {
                word.push(c);
            }
        }
        if !word.is_empty() {
            out.push(RawWord {
                text: word,
                exclude,
                quoted,
            });
        } else {
            // A dangling `-`/quote produces no word (for example `foo -`);
            // keep the marker so the trailing-ignored logic in `parse_query`
            // sees it instead of flipping the preceding word to a prefix.
            out.push(RawWord {
                text: String::new(),
                exclude,
                quoted,
            });
        }
    }
    out
}
