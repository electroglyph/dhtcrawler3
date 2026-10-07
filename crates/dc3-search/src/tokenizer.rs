//! The `dc3` and `dc3_cjk1` tokenizers.
//!
//! Both normalise the same way: NFKC, lowercase, split on non-alphanumerics.
//! They need no dictionary, so they work the same for every language (R17).
//!
//! * `dc3` ([`Dc3Tokenizer`]) turns CJK runs into overlapping bigrams and
//!   ASCII-folds everything else. Positions increase by one per token, so
//!   overlapping bigrams sit at consecutive positions and a phrase query over
//!   them matches a CJK substring.
//! * `dc3_cjk1` ([`Cjk1Tokenizer`]) emits every CJK character as its own
//!   token and skips everything else. It feeds the `*_cjk1` fields, which
//!   answer one-character CJK queries.
//!
//! Offsets point into the original text.

use tantivy::tokenizer::{
    AsciiFoldingFilter, RawTokenizer, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Name under which [`Dc3Tokenizer`] is registered with Tantivy.
pub const TOKENIZER_NAME: &str = "dc3";

/// Name under which [`Cjk1Tokenizer`] is registered with Tantivy.
pub const CJK1_TOKENIZER_NAME: &str = "dc3_cjk1";

/// Tokens longer than this many bytes (after normalisation) are dropped.
pub const MAX_TOKEN_BYTES: usize = 64;

// The shared CJK definition lives in [`dc3_core::text`]; re-exported so
// `crate::is_cjk` keeps resolving for existing users.
pub use dc3_core::text::is_cjk;

/// True when `s` contains at least one CJK character.
pub fn contains_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// True when `token` is one CJK character, optionally followed by combining
/// marks. [`Cjk1Tokenizer`] emits only such tokens, and [`Dc3Tokenizer`]
/// emits one for a lone CJK character.
pub fn is_cjk_unigram(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next().is_some_and(is_cjk) && chars.all(is_combining_mark)
}

/// The `dc3` tokenizer. Cheap to clone; register it with
/// [`TOKENIZER_NAME`].
#[derive(Clone)]
pub struct Dc3Tokenizer {
    folder: TextAnalyzer,
    buffer: Vec<Token>,
}

impl std::fmt::Debug for Dc3Tokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dc3Tokenizer").finish_non_exhaustive()
    }
}

impl Default for Dc3Tokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Dc3Tokenizer {
    pub fn new() -> Self {
        Self {
            folder: TextAnalyzer::builder(RawTokenizer::default())
                .filter(AsciiFoldingFilter)
                .build(),
            buffer: Vec::new(),
        }
    }

    /// Tokenizes `text` and returns every token.
    pub fn tokens(&mut self, text: &str) -> Vec<Token> {
        let mut out = Vec::new();
        analyze(Mode::Words(&mut self.folder), text, &mut out);
        out
    }

    /// Tokenizes `text` and returns only the token texts.
    pub fn token_texts(&mut self, text: &str) -> Vec<String> {
        self.tokens(text).into_iter().map(|t| t.text).collect()
    }
}

impl Tokenizer for Dc3Tokenizer {
    type TokenStream<'a> = Dc3TokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Dc3TokenStream<'a> {
        let Self { folder, buffer } = self;
        analyze(Mode::Words(folder), text, buffer);
        Dc3TokenStream::new(buffer)
    }
}

/// The `dc3_cjk1` tokenizer: one token per CJK character, nothing else.
/// Cheap to clone; register it with [`CJK1_TOKENIZER_NAME`].
#[derive(Clone, Default)]
pub struct Cjk1Tokenizer {
    buffer: Vec<Token>,
}

impl std::fmt::Debug for Cjk1Tokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cjk1Tokenizer").finish_non_exhaustive()
    }
}

impl Cjk1Tokenizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tokenizes `text` and returns every token.
    pub fn tokens(&mut self, text: &str) -> Vec<Token> {
        let mut out = Vec::new();
        analyze(Mode::CjkUnigrams, text, &mut out);
        out
    }

    /// Tokenizes `text` and returns only the token texts.
    pub fn token_texts(&mut self, text: &str) -> Vec<String> {
        self.tokens(text).into_iter().map(|t| t.text).collect()
    }
}

impl Tokenizer for Cjk1Tokenizer {
    type TokenStream<'a> = Dc3TokenStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Dc3TokenStream<'a> {
        analyze(Mode::CjkUnigrams, text, &mut self.buffer);
        Dc3TokenStream::new(&mut self.buffer)
    }
}

/// Token stream produced by [`Dc3Tokenizer`] and [`Cjk1Tokenizer`].
pub struct Dc3TokenStream<'a> {
    tokens: &'a mut Vec<Token>,
    next: usize,
    current: Token,
}

impl<'a> Dc3TokenStream<'a> {
    fn new(tokens: &'a mut Vec<Token>) -> Self {
        Self {
            tokens,
            next: 0,
            current: Token::default(),
        }
    }
}

impl TokenStream for Dc3TokenStream<'_> {
    fn advance(&mut self) -> bool {
        match self.tokens.get_mut(self.next) {
            Some(tok) => {
                self.current = std::mem::take(tok);
                self.next = self.next.saturating_add(1);
                true
            }
            None => false,
        }
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

/// What the analyzer emits.
enum Mode<'a> {
    /// `dc3`: folded words and CJK bigrams.
    Words(&'a mut TextAnalyzer),
    /// `dc3_cjk1`: CJK characters only.
    CjkUnigrams,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Cjk,
    Other,
}

/// One normalised character with the byte range of the original text it
/// came from.
#[derive(Clone, Copy)]
struct NormChar {
    c: char,
    from: usize,
    to: usize,
    mark: bool,
}

struct Analyzer<'a> {
    mode: Mode<'a>,
    out: &'a mut Vec<Token>,
    position: usize,
    segment: Vec<NormChar>,
    class: Class,
}

fn analyze<'a>(mode: Mode<'a>, text: &str, out: &'a mut Vec<Token>) {
    out.clear();
    let mut a = Analyzer {
        mode,
        out,
        position: 0,
        segment: Vec::new(),
        class: Class::Other,
    };
    // Normalise one cluster (a character plus its trailing combining marks)
    // at a time so every output character can be mapped back to an original
    // byte range; composition happens within the cluster.
    let mut cluster_start = 0usize;
    for (i, c) in text.char_indices() {
        if i > cluster_start && !is_combining_mark(c) {
            a.push_cluster(text, cluster_start, i);
            cluster_start = i;
        }
    }
    if cluster_start < text.len() {
        a.push_cluster(text, cluster_start, text.len());
    }
    a.flush();
}

impl Analyzer<'_> {
    fn push_cluster(&mut self, text: &str, from: usize, to: usize) {
        let Some(cluster) = text.get(from..to) else {
            return;
        };
        for n in cluster.nfkc() {
            for c in n.to_lowercase() {
                self.push_char(c, from, to);
            }
        }
    }

    fn push_char(&mut self, c: char, from: usize, to: usize) {
        let mark = is_combining_mark(c);
        if !(mark || c.is_alphanumeric()) {
            self.flush();
            return;
        }
        let nc = NormChar { c, from, to, mark };
        if mark {
            // Marks attach to whatever precedes them; a leading mark is dropped.
            if !self.segment.is_empty() {
                self.segment.push(nc);
            }
            return;
        }
        let class = if is_cjk(c) { Class::Cjk } else { Class::Other };
        if class != self.class && !self.segment.is_empty() {
            self.flush();
        }
        self.class = class;
        self.segment.push(nc);
    }

    fn flush(&mut self) {
        if self.segment.is_empty() {
            return;
        }
        let segment = std::mem::take(&mut self.segment);
        match self.class {
            Class::Other => self.emit_word(&segment),
            Class::Cjk => self.emit_cjk(&segment),
        }
    }

    fn emit_word(&mut self, chars: &[NormChar]) {
        let Mode::Words(folder) = &mut self.mode else {
            return;
        };
        let raw: String = chars.iter().map(|n| n.c).collect();
        let folded = if raw.is_ascii() {
            raw
        } else {
            let mut stream = folder.token_stream(&raw);
            let folded = match stream.next() {
                Some(tok) => tok.text.clone(),
                None => String::new(),
            };
            folded
                .chars()
                .filter(|c| c.is_alphanumeric() || is_combining_mark(*c))
                .collect()
        };
        if let (Some(first), Some(last)) = (chars.first(), chars.last()) {
            self.emit(folded, first.from, last.to);
        }
    }

    fn emit_cjk(&mut self, chars: &[NormChar]) {
        // Group into units: a base character plus its trailing marks.
        let mut units: Vec<(String, usize, usize)> = Vec::new();
        for n in chars {
            match units.last_mut() {
                Some(unit) if n.mark => {
                    unit.0.push(n.c);
                    unit.2 = n.to;
                }
                _ => units.push((n.c.to_string(), n.from, n.to)),
            }
        }
        let bigrams = matches!(self.mode, Mode::Words(_));
        if !bigrams || units.len() == 1 {
            for (text, from, to) in units {
                self.emit(text, from, to);
            }
            return;
        }
        for pair in units.windows(2) {
            if let [a, b] = pair {
                let mut text = String::with_capacity(a.0.len().saturating_add(b.0.len()));
                text.push_str(&a.0);
                text.push_str(&b.0);
                self.emit(text, a.1, b.2);
            }
        }
    }

    fn emit(&mut self, text: String, from: usize, to: usize) {
        if text.is_empty() || text.len() > MAX_TOKEN_BYTES {
            // Dropped tokens still advance the position so a gap remains
            // between the neighbours: otherwise `x <long> ok` yields
            // adjacent positions and a phrase query `"x ok"` falsely matches.
            self.position = self.position.saturating_add(1);
            return;
        }
        self.out.push(Token {
            offset_from: from,
            offset_to: to.max(from),
            position: self.position,
            text,
            position_length: 1,
        });
        self.position = self.position.saturating_add(1);
    }
}
