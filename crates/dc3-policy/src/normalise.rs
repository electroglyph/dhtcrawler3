//! Text normalisation and tokenisation for blocked-term matching.
//!
//! The character pipeline is, in order:
//! NFKC → lowercase → drop default-ignorable code points → UTS #39 confusable
//! skeleton (non-ASCII, non-CJK characters only) → drop combining diacritics → NFC →
//! lowercase. It is repeated until the text stops changing (at most
//! [`MAX_NORMALISE_PASSES`] passes), which makes [`normalise`] idempotent.
//!
//! The result is then split into tokens. Four token-sequence variants are
//! derived from it (see [`Variants`]).

use unicode_normalization::UnicodeNormalization;
use unicode_script::{Script, UnicodeScript};

/// Maximum number of passes of the character pipeline while looking for a
/// fixed point. Real text converges after one or two.
pub const MAX_NORMALISE_PASSES: usize = 4;

/// Characters that act as letters during tokenisation so that leetspeak
/// such as `r@ygold` stays in one token. They are removed from base tokens.
const LEET_LETTERS: [char; 2] = ['@', '$'];

/// Leetspeak substitutions applied in tokens that contain a letter.
const LEET_MAP: [(char, char); 8] = [
    ('0', 'o'),
    ('1', 'i'),
    ('3', 'e'),
    ('4', 'a'),
    ('5', 's'),
    ('7', 't'),
    ('@', 'a'),
    ('$', 's'),
];

/// Unicode Default_Ignorable_Code_Point ranges (inclusive), hard-coded.
const DEFAULT_IGNORABLE: [(char, char); 17] = [
    ('\u{00AD}', '\u{00AD}'),
    ('\u{034F}', '\u{034F}'),
    ('\u{061C}', '\u{061C}'),
    ('\u{115F}', '\u{1160}'),
    ('\u{17B4}', '\u{17B5}'),
    ('\u{180B}', '\u{180F}'),
    ('\u{200B}', '\u{200F}'),
    ('\u{202A}', '\u{202E}'),
    ('\u{2060}', '\u{206F}'),
    ('\u{3164}', '\u{3164}'),
    ('\u{FE00}', '\u{FE0F}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFA0}', '\u{FFA0}'),
    ('\u{FFF0}', '\u{FFF8}'),
    ('\u{1BCA0}', '\u{1BCA3}'),
    ('\u{1D173}', '\u{1D17A}'),
    ('\u{E0000}', '\u{E0FFF}'),
];

/// Blocks of generic combining diacritical marks (inclusive). They are
/// dropped after the skeleton step (which is NFD), so `ç` folds to `c` and
/// marks stacked on a term (`p̲t̲h̲c̲`) cannot split it.
const COMBINING_DIACRITICS: [(char, char); 5] = [
    ('\u{0300}', '\u{036F}'),
    ('\u{1AB0}', '\u{1AFF}'),
    ('\u{1DC0}', '\u{1DFF}'),
    ('\u{20D0}', '\u{20FF}'),
    ('\u{FE20}', '\u{FE2F}'),
];

fn in_ranges(c: char, ranges: &[(char, char)]) -> bool {
    ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
}

/// True for Unicode Default_Ignorable_Code_Point characters.
fn is_default_ignorable(c: char) -> bool {
    in_ranges(c, &DEFAULT_IGNORABLE)
}

fn is_combining_diacritic(c: char) -> bool {
    in_ranges(c, &COMBINING_DIACRITICS)
}

/// True for characters that become single-character tokens.
fn is_cjk(c: char) -> bool {
    matches!(
        c.script(),
        Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul
    )
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || LEET_LETTERS.contains(&c)
}

/// One pass of the character pipeline.
fn pipeline_pass(text: &str) -> String {
    let folded: String = text
        .nfkc()
        .flat_map(char::to_lowercase)
        .filter(|&c| !is_default_ignorable(c))
        .collect();
    let mut skeleton = String::with_capacity(folded.len());
    let mut buf = [0u8; 4];
    for c in folded.chars() {
        if c.is_ascii() || is_cjk(c) {
            // The UTS #39 prototypes fold ASCII too (`m` → `rn`, `0` → `O`,
            // `1` → `l`); that would break whole-token matching of ordinary
            // words and numbers, so ASCII is left alone. CJK characters are
            // left alone as well: the skeleton splits Hangul syllables into
            // jamo that do not recompose, and maps Katakana to Han.
            skeleton.push(c);
        } else {
            let s: &str = c.encode_utf8(&mut buf);
            skeleton.extend(unicode_security::skeleton(s).filter(|&m| !is_combining_diacritic(m)));
        }
    }
    skeleton
        .nfc()
        .flat_map(char::to_lowercase)
        // Lowercasing can reintroduce nothing ignorable, but the NFC step can
        // in theory expose one; filter once more for a clean fixed point.
        .filter(|&c| !is_default_ignorable(c))
        .collect()
}

/// Runs the character pipeline to a fixed point (bounded).
fn fold(text: &str) -> String {
    let mut current = pipeline_pass(text);
    for _ in 1..MAX_NORMALISE_PASSES {
        let next = pipeline_pass(&current);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

/// Splits folded text into raw tokens ('@' and '$' still present).
fn tokenise(folded: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in folded.chars() {
        if is_cjk(c) {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(c.to_string());
        } else if is_word_char(c) {
            current.push(c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn strip_leet_letters(s: &str) -> String {
    s.chars().filter(|c| !LEET_LETTERS.contains(c)).collect()
}

/// Appends the base form of a raw token: '@' and '$' removed. Removing them
/// can join characters that normalise differently together (for example
/// combining marks that reorder), so such a token is folded and tokenised
/// again to keep [`normalise`] idempotent.
fn push_base_tokens(raw: &str, out: &mut Vec<String>) {
    if !raw.contains(LEET_LETTERS) {
        out.push(raw.to_owned());
        return;
    }
    let stripped = strip_leet_letters(raw);
    if stripped.is_empty() {
        return;
    }
    for t in tokenise(&fold(&stripped)) {
        let t = strip_leet_letters(&t);
        if !t.is_empty() {
            out.push(t);
        }
    }
}

fn base_tokens(raw: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(raw.len());
    for t in raw {
        push_base_tokens(t, &mut out);
    }
    out
}

/// Leet form of a raw token. Tokens with no letter (for example years) keep
/// their base form.
fn leet_tokens(raw: &str) -> Vec<String> {
    if !raw.chars().any(char::is_alphabetic) {
        let mut base = Vec::new();
        push_base_tokens(raw, &mut base);
        return base;
    }
    vec![
        raw.chars()
            .map(|c| {
                LEET_MAP
                    .iter()
                    .find(|&&(from, _)| from == c)
                    .map_or(c, |&(_, to)| to)
            })
            .collect(),
    ]
}

/// Splits a token at letter↔digit boundaries.
fn split_letter_digit(token: &str, out: &mut Vec<String>) {
    let mut current = String::new();
    let mut prev_digit: Option<bool> = None;
    for c in token.chars() {
        let digit = c.is_numeric();
        if prev_digit.is_some_and(|p| p != digit) && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push(c);
        prev_digit = Some(digit);
    }
    if !current.is_empty() {
        out.push(current);
    }
}

fn split_all(tokens: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        split_letter_digit(t, &mut out);
    }
    out
}

/// The token-sequence variants of one text that a term is matched against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Variants {
    /// (a) base tokens.
    pub base: Vec<String>,
    /// (b) leet-mapped tokens.
    pub leet: Vec<String>,
    /// (c) base tokens split at letter↔digit boundaries.
    pub split: Vec<String>,
    /// (d) leet tokens split at letter↔digit boundaries.
    pub leet_split: Vec<String>,
}

impl Variants {
    pub(crate) fn of(text: &str) -> Variants {
        let raw = tokenise(&fold(text));
        let base = base_tokens(&raw);
        let leet: Vec<String> = raw.iter().flat_map(|t| leet_tokens(t)).collect();
        let split = split_all(&base);
        let leet_split = split_all(&leet);
        Variants {
            base,
            leet,
            split,
            leet_split,
        }
    }

    pub(crate) fn all(&self) -> [&[String]; 4] {
        [&self.base, &self.leet, &self.split, &self.leet_split]
    }
}

/// Normalises text into base tokens: NFKC, lowercase, default-ignorables
/// removed, confusables folded to their UTS #39 skeleton, split on
/// non-alphanumerics, with each Han/Hiragana/Katakana/Hangul character as its
/// own token. Never panics; idempotent on `normalise(t).join(" ")`.
pub fn normalise(text: &str) -> Vec<String> {
    base_tokens(&tokenise(&fold(text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_tokens() {
        assert_eq!(normalise("Ubuntu 26.04 LTS"), ["ubuntu", "26", "04", "lts"]);
        assert_eq!(normalise("  "), Vec::<String>::new());
    }

    #[test]
    fn years_stay_years() {
        assert_eq!(normalise("2024"), ["2024"]);
        let v = Variants::of("2024 m1");
        assert_eq!(v.leet, ["2024", "mi"]);
        assert_eq!(v.split, ["2024", "m", "1"]);
    }

    #[test]
    fn ascii_not_skeletonised() {
        assert_eq!(normalise("mcp cowboy 1080p"), ["mcp", "cowboy", "1080p"]);
    }

    #[test]
    fn fullwidth_and_cyrillic_fold() {
        assert_eq!(normalise("ＰＴＨＣ"), ["pthc"]);
        assert_eq!(normalise("\u{0440}th\u{0441}"), ["pthc"]);
    }

    #[test]
    fn ignorables_and_diacritics_removed() {
        assert_eq!(normalise("pt\u{200D}h\u{00AD}c"), ["pthc"]);
        assert_eq!(normalise("p\u{0332}t\u{0332}h\u{0332}c\u{0332}"), ["pthc"]);
        assert_eq!(normalise("café"), ["cafe"]);
    }

    #[test]
    fn cjk_single_char_tokens() {
        assert_eq!(normalise("東京abc"), ["東", "京", "abc"]);
        assert_eq!(
            normalise("ひらがなカタ한국"),
            ["ひ", "ら", "が", "な", "カ", "タ", "한", "국"]
        );
    }

    #[test]
    fn leet_letters_removed_in_base() {
        assert_eq!(normalise("r@ygold $ @"), ["rygold"]);
        assert_eq!(Variants::of("r@ygold").leet, ["raygold"]);
        assert_eq!(Variants::of("$100").leet, ["100"]);
    }
}
