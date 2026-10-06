//! Sanitising attacker-authored text.
//!
//! Every torrent name and file path comes from whoever published the torrent
//! (axiom A2 in `docs/01-first-principles.md`). Before such text is stored,
//! indexed or displayed it is normalised and stripped of characters that can
//! hide or disguise content. HTML escaping is *not* done here; that is the
//! template engine's job at output time.

use unicode_normalization::UnicodeNormalization;

/// True for the Unicode bidirectional formatting controls that can reorder how
/// text is displayed (e.g. make `gpj.exe` render as `exe.jpg`).
pub fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// True for characters that should never appear in displayed metadata:
/// C0/C1 controls (except whitespace, which is collapsed separately), bidi
/// controls, zero-width characters, the BOM and Unicode non-characters.
/// Shared with report cleaning, so operator-visible text is held to the
/// same standard as indexed text.
pub fn is_unwanted(c: char) -> bool {
    if is_bidi_control(c) {
        return true;
    }
    if c.is_control() && !c.is_whitespace() {
        return true;
    }
    matches!(
        c,
        '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{FDD0}'..='\u{FDEF}'
    ) || (c as u32 & 0xFFFE) == 0xFFFE
}

/// Normalises (NFC), removes unwanted characters, collapses runs of whitespace
/// to one space, trims, and truncates to at most `max_chars` characters on a
/// character boundary.
pub fn sanitize_display(s: &str, max_chars: usize) -> String {
    sanitize_display_inner(s, max_chars, false)
}

fn sanitize_display_inner(s: &str, max_chars: usize, map_separators: bool) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars.saturating_mul(4)));
    let mut count = 0usize;
    let mut pending_space = false;
    for mut c in s.nfc() {
        // Fuse path-separator replacement into the display pass. `/` and `\`
        // are ASCII and NFC-stable (never a composition source or target),
        // so mapping after NFC equals mapping before it — one walk, one alloc.
        if map_separators && (c == '/' || c == '\\') {
            c = '_';
        }
        if is_unwanted(c) {
            continue;
        }
        if c.is_whitespace() {
            pending_space = count > 0;
            continue;
        }
        if pending_space {
            // Only emit the space if the character after it also fits, so a
            // truncated result never ends in whitespace.
            if count.saturating_add(1) >= max_chars {
                break;
            }
            out.push(' ');
            count = count.saturating_add(1);
            pending_space = false;
        }
        if count >= max_chars {
            break;
        }
        out.push(c);
        count = count.saturating_add(1);
    }
    out
}

/// Maximum characters kept from a single path component.
pub const PATH_COMPONENT_MAX_CHARS: usize = 255;

/// Sanitises one component of a file path from a torrent.
///
/// Returns `None` for components that must not appear in a path: empty, `.`
/// and `..` (directory traversal, BEP 52). Separators inside a component are
/// replaced so a single component can never introduce extra path levels.
pub fn sanitize_path_component(s: &str) -> Option<String> {
    let clean = sanitize_display_inner(s, PATH_COMPONENT_MAX_CHARS, true);
    match clean.as_str() {
        "" | "." | ".." => None,
        _ => Some(clean),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bidi_and_controls() {
        assert_eq!(sanitize_display("evil\u{202E}gpj.exe", 100), "evilgpj.exe");
        assert_eq!(sanitize_display("a\u{0000}b\u{0007}c", 100), "abc");
        assert_eq!(sanitize_display("zero\u{200B}width", 100), "zerowidth");
        assert_eq!(sanitize_display("\u{FEFF}bom", 100), "bom");
    }

    #[test]
    fn collapses_whitespace_and_trims() {
        assert_eq!(sanitize_display("  a \t\n  b  ", 100), "a b");
        assert_eq!(sanitize_display("   ", 100), "");
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(sanitize_display("東京大学", 2), "東京");
        assert_eq!(sanitize_display("ab cd", 2), "ab");
        assert_eq!(sanitize_display("ab cd", 3), "ab");
        assert_eq!(sanitize_display("ab cd", 4), "ab c");
    }

    #[test]
    fn normalises_to_nfc() {
        // "e" + combining acute -> "é"
        assert_eq!(sanitize_display("e\u{0301}", 10), "\u{00E9}");
    }

    #[test]
    fn keeps_html_metacharacters_for_the_template_to_escape() {
        assert_eq!(sanitize_display("<script>", 100), "<script>");
    }

    #[test]
    fn path_components() {
        assert_eq!(sanitize_path_component(".."), None);
        assert_eq!(sanitize_path_component("."), None);
        assert_eq!(sanitize_path_component(""), None);
        assert_eq!(sanitize_path_component(" \u{202E} "), None);
        assert_eq!(sanitize_path_component("a/../b").as_deref(), Some("a_.._b"));
        assert_eq!(sanitize_path_component("x\\y").as_deref(), Some("x_y"));
        assert_eq!(
            sanitize_path_component("movie.mkv").as_deref(),
            Some("movie.mkv")
        );
    }

    #[test]
    fn noncharacters_removed() {
        assert_eq!(sanitize_display("a\u{FFFE}b\u{FFFF}c\u{1FFFE}", 10), "abc");
    }
}
