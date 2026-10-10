//! JSON output that is also safe to embed in HTML.
//!
//! `serde_json` escapes only what JSON requires. This formatter also writes
//! `<`, `>`, `&`, `'`, U+2028 and U+2029 as `\u` escapes, so a response body
//! never contains markup such as `<script` even when a torrent name does.
//! The JSON value is unchanged.

use std::io;

use serde::Serialize;
use serde_json::ser::{Formatter, Serializer};

/// Serialises `value` as compact JSON with HTML-significant characters
/// escaped.
pub(crate) fn to_vec<T: Serialize + ?Sized>(value: &T) -> serde_json::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut ser = Serializer::with_formatter(&mut out, HtmlSafeFormatter);
    value.serialize(&mut ser)?;
    Ok(out)
}

struct HtmlSafeFormatter;

impl Formatter for HtmlSafeFormatter {
    fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> io::Result<()>
    where
        W: ?Sized + io::Write,
    {
        let mut start = 0usize;
        for (i, c) in fragment.char_indices() {
            let escaped = match c {
                '<' => "\\u003c",
                '>' => "\\u003e",
                '&' => "\\u0026",
                '\'' => "\\u0027",
                '\u{2028}' => "\\u2028",
                '\u{2029}' => "\\u2029",
                _ => continue,
            };
            writer.write_all(fragment.get(start..i).unwrap_or_default().as_bytes())?;
            writer.write_all(escaped.as_bytes())?;
            start = i.saturating_add(c.len_utf8());
        }
        writer.write_all(fragment.get(start..).unwrap_or_default().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Sample<'a> {
        name: &'a str,
    }

    #[test]
    fn escapes_markup_but_keeps_the_value() {
        let name = "<script>alert('x')</script> & \u{2028}\"\n東京";
        let bytes = to_vec(&Sample { name }).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains('<'));
        assert!(!text.contains('>'));
        assert!(!text.contains('&'));
        assert!(!text.contains('\''));
        assert!(!text.contains('\u{2028}'));
        assert!(text.contains("\\u003cscript\\u003e"));
        assert!(text.contains("東京"));
        let back: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back["name"], name);
    }
}
