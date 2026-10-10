//! Turning metadata byte strings into Rust strings.
//!
//! BEP 3 says text is UTF-8, but many old torrents use a legacy code page,
//! sometimes named by the info dictionary's `encoding` key. Decoding here is
//! lossless when possible and lossy as a last resort; sanitising happens after.

use std::borrow::Cow;

use encoding_rs::{Encoding, GB18030};

/// Decodes `bytes`: UTF-8 as is; else the `encoding` label, then GB18030,
/// accepting either only when every byte decodes; else lossy UTF-8.
pub(crate) fn decode_text<'a>(bytes: &'a [u8], label: Option<&'static Encoding>) -> Cow<'a, str> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Cow::Borrowed(s);
    }
    if let Some(enc) = label
        && let Some(s) = enc.decode_without_bom_handling_and_without_replacement(bytes)
    {
        return s;
    }
    if let Some(s) = GB18030.decode_without_bom_handling_and_without_replacement(bytes) {
        return s;
    }
    String::from_utf8_lossy(bytes)
}

/// Resolves an `encoding` label (WHATWG rules); unknown labels are ignored.
pub(crate) fn label_encoding(label: Option<&[u8]>) -> Option<&'static Encoding> {
    label.and_then(Encoding::for_label)
}
