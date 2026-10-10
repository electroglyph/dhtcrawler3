//! The stylesheet and the plain-text files, embedded in the binary.

use std::sync::LazyLock;

/// The stylesheet.
pub(crate) const STYLE_CSS: &str = include_str!("../static/style.css");

/// FNV-1a hash of the stylesheet, computed at build time; it names the
/// cache-busting stylesheet URL.
pub(crate) const STYLE_HASH: u64 = fnv1a64(STYLE_CSS.as_bytes());

/// `robots.txt`: search engines stay out of every dynamic page.
pub(crate) const ROBOTS_TXT: &str = "User-agent: *\n\
Disallow: /search\n\
Disallow: /api/\n\
Disallow: /t/\n";

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

static STYLE_PATH: LazyLock<String> =
    LazyLock::new(|| format!("/static/style.{STYLE_HASH:016x}.css"));

/// The path of the stylesheet whose name carries its hash; it may be cached
/// forever.
pub(crate) fn style_path() -> &'static str {
    STYLE_PATH.as_str()
}

const fn fnv1a64(mut bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    while let Some((first, rest)) = bytes.split_first() {
        hash ^= *first as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        bytes = rest;
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_reference_values() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn style_path_has_the_hash() {
        let path = style_path();
        assert!(path.starts_with("/static/style."));
        assert!(path.ends_with(".css"));
        assert_eq!(path.len(), "/static/style.".len() + 16 + ".css".len());
    }

    #[test]
    fn stylesheet_has_no_external_references() {
        assert!(!STYLE_CSS.contains("url("));
        assert!(!STYLE_CSS.contains("@import"));
        // Dark by default; the server-rendered `data-theme="light"`
        // selects light without JavaScript.
        assert!(STYLE_CSS.contains("--bg: #0f1216"));
        assert!(STYLE_CSS.contains(":root[data-theme=\"light\"]"));
        assert!(STYLE_CSS.contains("--bg: #ffffff"));
        assert!(!STYLE_CSS.contains("prefers-color-scheme"));
        assert!(!STYLE_CSS.contains(":has(#theme-toggle"));
    }
}
