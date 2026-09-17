//! Content policy for dhtcrawler3 (R18): blocked-term matching.
//!
//! * [`normalise`]: folds text (NFKC, case, default-ignorables, UTS #39
//!   confusables) and splits it into whole tokens.
//! * [`TermMatcher`]: matches whole tokens or whole phrases from a term list
//!   against text, also trying leetspeak and letter/digit-split variants.
//! * [`SEED_TERMS`] / [`seed`]: the list shipped in `policy/blocked-terms.txt`.
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]

mod normalise;

use std::collections::HashMap;

use normalise::Variants;
pub use normalise::{MAX_NORMALISE_PASSES, normalise};

/// The seed blocked-term list shipped with the repository.
pub const SEED_TERMS: &str = include_str!("../../../policy/blocked-terms.txt");

/// Maximum number of characters in one term line (after removing comments).
pub const MAX_TERM_LINE_CHARS: usize = 1024;
/// Maximum number of tokens in one term (phrase).
pub const MAX_TERM_TOKENS: usize = 16;
/// Maximum number of distinct terms a matcher holds.
pub const MAX_TERMS: usize = 1_000_000;

/// Character that starts a comment in a term list.
const COMMENT_CHAR: char = '#';

/// Error returned when loading a term list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("line {line}: term is longer than {max} characters")]
    LineTooLong { line: usize, max: usize },
    #[error("line {line}: term has no letters or digits after normalisation")]
    EmptyTerm { line: usize },
    #[error("line {line}: term has more than {max} tokens")]
    TooManyTokens { line: usize, max: usize },
    #[error("more than {max} terms")]
    TooManyTerms { max: usize },
}

/// A set of blocked terms, each a sequence of base-normalised tokens.
///
/// Terms are indexed by their first token. Matching a text of `n` tokens
/// costs O(n · c · L) per variant (four variants), where `c` is the number
/// of terms sharing a first token and `L` the longest term length; for real
/// lists `c` and `L` are small, so matching is linear in the text.
#[derive(Debug, Clone, Default)]
pub struct TermMatcher {
    /// First token → full token sequences starting with it.
    index: HashMap<String, Vec<Vec<String>>>,
    len: usize,
}

impl TermMatcher {
    /// Parses a term list: one term per line, `#` starts a comment, blank
    /// lines are ignored, and a line with several words is a phrase.
    /// Duplicate terms are kept once.
    pub fn load(text: &str) -> Result<TermMatcher, PolicyError> {
        let mut matcher = TermMatcher::empty();
        for (i, line) in text.lines().enumerate() {
            let line_no = i.saturating_add(1);
            let content = line
                .split_once(COMMENT_CHAR)
                .map_or(line, |(before, _)| before);
            if content.trim().is_empty() {
                continue;
            }
            if content.chars().count() > MAX_TERM_LINE_CHARS {
                return Err(PolicyError::LineTooLong {
                    line: line_no,
                    max: MAX_TERM_LINE_CHARS,
                });
            }
            let tokens = normalise(content);
            if tokens.len() > MAX_TERM_TOKENS {
                return Err(PolicyError::TooManyTokens {
                    line: line_no,
                    max: MAX_TERM_TOKENS,
                });
            }
            let Some(first) = tokens.first().cloned() else {
                return Err(PolicyError::EmptyTerm { line: line_no });
            };
            let bucket = matcher.index.entry(first).or_default();
            if bucket.contains(&tokens) {
                continue;
            }
            if matcher.len >= MAX_TERMS {
                return Err(PolicyError::TooManyTerms { max: MAX_TERMS });
            }
            bucket.push(tokens);
            matcher.len = matcher.len.saturating_add(1);
        }
        Ok(matcher)
    }

    /// A matcher with no terms; it matches nothing.
    pub fn empty() -> TermMatcher {
        TermMatcher::default()
    }

    /// True if any term appears as a contiguous whole-token sequence in the
    /// base, leetspeak or letter/digit-split tokens of `text`.
    pub fn matches(&self, text: &str) -> bool {
        if self.index.is_empty() {
            return false;
        }
        let variants = Variants::of(text);
        variants
            .all()
            .iter()
            .any(|tokens| self.matches_tokens(tokens))
    }

    fn matches_tokens(&self, tokens: &[String]) -> bool {
        tokens.iter().enumerate().any(|(i, tok)| {
            let Some(candidates) = self.index.get(tok) else {
                return false;
            };
            let Some(rest) = tokens.get(i..) else {
                return false;
            };
            candidates.iter().any(|term| rest.starts_with(term))
        })
    }

    /// Number of distinct terms.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if the matcher holds no terms.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The matcher for [`SEED_TERMS`].
///
/// This convenience form falls back to an empty matcher if the shipped list
/// were ever invalid (the test suite guards against that). Services must use
/// [`try_seed`] so that a broken list stops startup instead of silently
/// disabling the filter (fail closed).
pub fn seed() -> TermMatcher {
    try_seed().unwrap_or_default()
}

/// Loads the built-in seed list, returning an error instead of an empty
/// matcher if it cannot be parsed.
pub fn try_seed() -> Result<TermMatcher, PolicyError> {
    TermMatcher::load(SEED_TERMS)
}
