//! Shared types for dhtcrawler4.
//!
//! * [`DhtKey`]: the 20-byte key under which a torrent is found in the DHT
//!   (a v1 infohash or a truncated v2 infohash; see `docs/00-horismos.md` §3).
//! * [`InfoHashV2`]: a full 32-byte BitTorrent v2 infohash.
//! * [`magnet_link`]: builds a magnet URI from validated hashes only.
//! * [`validate_base_url`]: the single `web.base_url` origin policy shared by
//!   the binary config validator and the web front end.
//! * [`text`]: sanitising of attacker-authored text (torrent names, paths).
#![forbid(unsafe_code)]

pub mod text;

use std::fmt;
use std::net::Ipv6Addr;
use std::str::FromStr;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum number of characters of a display name placed in a magnet link.
pub const MAGNET_DISPLAY_NAME_MAX_CHARS: usize = 200;

/// Largest `web.base_url`, in characters.
pub const MAX_BASE_URL_CHARS: usize = 2048;

/// Error returned when parsing a [`DhtKey`] or [`InfoHashV2`] from text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyParseError {
    #[error("expected 40 hex or 32 base32 characters, got {0} characters")]
    BadLength(usize),
    #[error("expected 64 hex characters, got {0} characters")]
    BadV2Length(usize),
    #[error("invalid hex")]
    BadHex,
    #[error("invalid base32")]
    BadBase32,
    #[error("expected {expected} bytes, got {got}")]
    BadByteLength { expected: usize, got: usize },
}

/// A 20-byte DHT key: a v1 infohash or the first 20 bytes of a v2 infohash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DhtKey(pub [u8; 20]);

impl DhtKey {
    pub const LEN: usize = 20;

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Builds a key from a byte slice that must be exactly 20 bytes long.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, KeyParseError> {
        let arr: [u8; 20] = bytes.try_into().map_err(|_| KeyParseError::BadByteLength {
            expected: Self::LEN,
            got: bytes.len(),
        })?;
        Ok(Self(arr))
    }

    /// Lowercase 40-character hex.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for DhtKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for DhtKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DhtKey({})", self.to_hex())
    }
}

impl FromStr for DhtKey {
    type Err = KeyParseError;

    /// Accepts 40 hex characters (any case) or 32 base32 characters (any case),
    /// as allowed in magnet links (BEP 9).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_counted(s, s.chars().count())
    }
}

impl DhtKey {
    /// Parses with a precomputed character count so callers that already
    /// counted (such as [`AnyKey`]) do not walk the string twice.
    fn parse_counted(s: &str, n: usize) -> Result<Self, KeyParseError> {
        match n {
            40 => {
                let mut out = [0u8; 20];
                hex::decode_to_slice(s, &mut out).map_err(|_| KeyParseError::BadHex)?;
                Ok(Self(out))
            }
            32 => {
                let upper = s.to_ascii_uppercase();
                let bytes = data_encoding::BASE32_NOPAD
                    .decode(upper.as_bytes())
                    .map_err(|_| KeyParseError::BadBase32)?;
                Self::from_slice(&bytes)
            }
            n => Err(KeyParseError::BadLength(n)),
        }
    }
}

impl Serialize for DhtKey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for DhtKey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A full 32-byte BitTorrent v2 infohash (SHA-256 of the info dictionary).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InfoHashV2(pub [u8; 32]);

impl InfoHashV2 {
    pub const LEN: usize = 32;

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, KeyParseError> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| KeyParseError::BadByteLength {
            expected: Self::LEN,
            got: bytes.len(),
        })?;
        Ok(Self(arr))
    }

    /// The first 20 bytes, which is how v2 torrents are keyed in the DHT (BEP 52).
    pub fn truncated(&self) -> DhtKey {
        let mut out = [0u8; 20];
        out.copy_from_slice(&self.0[..20]);
        DhtKey(out)
    }

    /// Lowercase 64-character hex.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for InfoHashV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for InfoHashV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHashV2({})", self.to_hex())
    }
}

impl FromStr for InfoHashV2 {
    type Err = KeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_counted(s, s.chars().count())
    }
}

impl InfoHashV2 {
    /// Parses with a precomputed character count so callers that already
    /// counted (such as [`AnyKey`]) do not walk the string twice.
    fn parse_counted(s: &str, n: usize) -> Result<Self, KeyParseError> {
        if n != 64 {
            return Err(KeyParseError::BadV2Length(n));
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(|_| KeyParseError::BadHex)?;
        Ok(Self(out))
    }
}

impl Serialize for InfoHashV2 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for InfoHashV2 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Either kind of torrent identifier a person may type or link to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnyKey {
    V1OrDht(DhtKey),
    V2(InfoHashV2),
}

impl FromStr for AnyKey {
    type Err = KeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Count once and reuse it: the delegated parsers would walk the
        // string again to measure it.
        let n = s.chars().count();
        if n == 64 {
            InfoHashV2::parse_counted(s, n).map(AnyKey::V2)
        } else {
            DhtKey::parse_counted(s, n).map(AnyKey::V1OrDht)
        }
    }
}

/// Characters left unescaped in a magnet `dn` value (RFC 3986 unreserved).
const MAGNET_DN_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Builds a magnet URI from validated hashes.
///
/// Emits `xt=urn:btih:<40 hex>` for v1 and `xt=urn:btmh:1220<64 hex>` for v2
/// (multihash prefix 0x12 = sha2-256, 0x20 = 32 bytes). Hybrids get both. The
/// display name is sanitised, truncated and percent-encoded. No trackers are
/// added: the DHT is the only discovery mechanism. Returns `None` when both
/// hashes are absent.
pub fn magnet_link(
    v1: Option<&DhtKey>,
    v2: Option<&InfoHashV2>,
    display_name: Option<&str>,
) -> Option<String> {
    if v1.is_none() && v2.is_none() {
        return None;
    }
    let mut out = String::from("magnet:?");
    let mut first = true;
    // Emits the `&` separator between parameters without building
    // intermediate strings for each part.
    let mut sep = |out: &mut String| {
        if !first {
            out.push('&');
        }
        first = false;
    };
    if let Some(k) = v1 {
        sep(&mut out);
        out.push_str("xt=urn:btih:");
        out.push_str(&k.to_hex());
    }
    if let Some(k) = v2 {
        sep(&mut out);
        out.push_str("xt=urn:btmh:1220");
        out.push_str(&k.to_hex());
    }
    if let Some(name) = display_name {
        let clean = text::sanitize_display(name, MAGNET_DISPLAY_NAME_MAX_CHARS);
        if !clean.is_empty() {
            sep(&mut out);
            out.push_str("dn=");
            out.push_str(&utf8_percent_encode(&clean, MAGNET_DN_ESCAPE).to_string());
        }
    }
    Some(out)
}

/// Checks that `url` is an `http(s)` origin: lowercase scheme and host, an
/// optional non-default port, and nothing else.
///
/// This is the single policy shared by the binary config validator and the
/// web front end, so the same value cannot pass one and fail the other.
/// Reasons are bare fragments; callers prefix them with the field name.
pub fn validate_base_url(url: &str, max_chars: usize) -> Result<(), String> {
    if url.chars().count() > max_chars {
        return Err(format!("is longer than {max_chars} characters"));
    }
    let (rest, default_port) = if let Some(rest) = url.strip_prefix("https://") {
        (rest, 443)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (rest, 80)
    } else {
        return Err("must start with http:// or https://".into());
    };
    if rest.is_empty() {
        return Err("has no host".into());
    }
    if rest.contains(['/', '?', '#', '@', '\\'])
        || rest.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(
            "must be an origin (scheme, host and optional port) without a path or trailing slash"
                .into(),
        );
    }
    if rest.chars().any(|c| c.is_ascii_uppercase()) {
        return Err("must be lowercase, as browsers send it in the Origin header".into());
    }
    let (host, port) = match rest.strip_prefix('[') {
        Some(inner) => {
            let (ip, after) = inner
                .split_once(']')
                .ok_or_else(|| "has an unterminated IPv6 literal".to_owned())?;
            Ipv6Addr::from_str(ip).map_err(|_| "has an invalid IPv6 literal".to_owned())?;
            let port = match after {
                "" => None,
                p => Some(
                    p.strip_prefix(':')
                        .ok_or_else(|| "has junk after the IPv6 literal".to_owned())?,
                ),
            };
            (ip, port)
        }
        None => match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        },
    };
    if host.is_empty() {
        return Err("has no host".into());
    }
    if !rest.starts_with('[')
        && !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        return Err("has an invalid host".into());
    }
    if let Some(port) = port {
        let port: u16 = port.parse().map_err(|_| "has an invalid port".to_owned())?;
        if port == 0 {
            return Err("has port 0".to_owned());
        }
        if port == default_port {
            return Err(format!(
                "must omit the default port {default_port}, as browsers do"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dht_key_hex_round_trip_and_case() {
        let s = "0123456789ABCDEF0123456789abcdef01234567";
        let k: DhtKey = s.parse().unwrap();
        assert_eq!(k.to_hex(), s.to_ascii_lowercase());
    }

    #[test]
    fn dht_key_base32() {
        let k: DhtKey = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
        let b32 = data_encoding::BASE32_NOPAD.encode(&k.0);
        assert_eq!(b32.len(), 32);
        let back: DhtKey = b32.to_ascii_lowercase().parse().unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn dht_key_rejects_bad_input() {
        assert_eq!("abc".parse::<DhtKey>(), Err(KeyParseError::BadLength(3)));
        assert_eq!(
            "zz23456789abcdef0123456789abcdef01234567".parse::<DhtKey>(),
            Err(KeyParseError::BadHex)
        );
        assert!("1".repeat(32).parse::<DhtKey>().is_err());
        assert!(DhtKey::from_slice(&[0u8; 19]).is_err());
    }

    #[test]
    fn key_length_errors_count_characters() {
        // 20 chars / 40 bytes: byte length hits the hex arm, char length does not.
        let twenty = "é".repeat(20);
        assert_eq!(twenty.chars().count(), 20);
        assert_eq!(twenty.len(), 40);
        assert_eq!(twenty.parse::<DhtKey>(), Err(KeyParseError::BadLength(20)));
        // 40 chars / 41 bytes: char length hits the hex arm, byte length does not.
        let s = format!("{}é{}", "a".repeat(19), "a".repeat(20));
        assert_eq!(s.chars().count(), 40);
        assert_eq!(s.len(), 41);
        assert_eq!(s.parse::<DhtKey>(), Err(KeyParseError::BadHex));
        // 64 chars / 65 bytes reports the true char count path, not bytes.
        let v2 = format!("{}é{}", "b".repeat(63), "");
        assert_eq!(v2.chars().count(), 64);
        assert_eq!(v2.parse::<InfoHashV2>(), Err(KeyParseError::BadHex));
        // AnyKey dispatches on char count too: 40-char multibyte hits the
        // hex arm, not the length error.
        assert_eq!(s.parse::<AnyKey>(), Err(KeyParseError::BadHex));
        // 32-char base32 arm uses char count: 32 multibyte chars (64 bytes)
        // hit the base32 arm, not the length error.
        let b32multi = "é".repeat(32);
        assert_eq!(b32multi.chars().count(), 32);
        assert_eq!(b32multi.len(), 64);
        assert_eq!(b32multi.parse::<DhtKey>(), Err(KeyParseError::BadBase32));
        // Non-64-char multibyte on the v2 path reports chars, not bytes.
        let v2short = "é".repeat(32);
        assert_eq!(
            v2short.parse::<InfoHashV2>(),
            Err(KeyParseError::BadV2Length(32))
        );
    }

    #[test]
    fn v2_truncation() {
        let mut b = [0u8; 32];
        for (i, x) in b.iter_mut().enumerate() {
            *x = i as u8;
        }
        let h = InfoHashV2(b);
        assert_eq!(h.truncated().0[..], b[..20]);
        assert_eq!(h.to_string().parse::<InfoHashV2>().unwrap(), h);
    }

    #[test]
    fn any_key_dispatch() {
        assert!(matches!(
            "a".repeat(40).parse::<AnyKey>(),
            Ok(AnyKey::V1OrDht(_))
        ));
        assert!(matches!(
            "b".repeat(64).parse::<AnyKey>(),
            Ok(AnyKey::V2(_))
        ));
    }

    #[test]
    fn any_key_single_count_covers_all_arms() {
        // Error lengths are reported from the one measurement.
        assert_eq!("abc".parse::<AnyKey>(), Err(KeyParseError::BadLength(3)));
        assert_eq!(
            "b".repeat(65).parse::<AnyKey>(),
            Err(KeyParseError::BadLength(65))
        );
        // A long invalid input reports its full character count.
        let long = "é".repeat(10_000);
        assert_eq!(
            long.parse::<AnyKey>(),
            Err(KeyParseError::BadLength(10_000))
        );
        assert_eq!(
            long.parse::<InfoHashV2>(),
            Err(KeyParseError::BadV2Length(10_000))
        );
        // The 32-char base32 arm is reachable through dispatch.
        let k: DhtKey = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
        let b32 = data_encoding::BASE32_NOPAD.encode(&k.0);
        assert_eq!(b32.parse::<AnyKey>(), Ok(AnyKey::V1OrDht(k)));
        // 64-char non-hex reaches the v2 decoder, not the v1 one.
        assert_eq!(
            "z".repeat(64).parse::<AnyKey>(),
            Err(KeyParseError::BadHex)
        );
    }

    #[test]
    fn magnet_v1_v2_and_name() {
        let v1: DhtKey = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
        let v2 = InfoHashV2([0xab; 32]);
        let m = magnet_link(Some(&v1), Some(&v2), Some("Ubuntu 26.04 <x>&\u{202E}gpj")).unwrap();
        assert_eq!(
            m,
            format!(
                "magnet:?xt=urn:btih:{}&xt=urn:btmh:1220{}&dn=Ubuntu%2026.04%20%3Cx%3E%26gpj",
                v1.to_hex(),
                "ab".repeat(32)
            )
        );
        assert_eq!(magnet_link(None, None, Some("x")), None);
        assert_eq!(
            magnet_link(Some(&v1), None, None).unwrap(),
            format!("magnet:?xt=urn:btih:{}", v1.to_hex())
        );
    }

    #[test]
    fn magnet_each_arm_and_separators() {
        let v1: DhtKey = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
        let v2 = InfoHashV2([0xab; 32]);
        // v2-only and name-only shapes.
        assert_eq!(
            magnet_link(None, Some(&v2), None).unwrap(),
            format!("magnet:?xt=urn:btmh:1220{}", "ab".repeat(32))
        );
        assert_eq!(
            magnet_link(None, Some(&v2), Some("plain name")).unwrap(),
            format!(
                "magnet:?xt=urn:btmh:1220{}&dn=plain%20name",
                "ab".repeat(32)
            )
        );
        // A display name that sanitises to nothing adds no parameter
        // and no trailing separator.
        assert_eq!(
            magnet_link(Some(&v1), None, Some("\u{202E}")).unwrap(),
            format!("magnet:?xt=urn:btih:{}", v1.to_hex())
        );
        // No hashes at all yields nothing, even with a name.
        assert_eq!(magnet_link(None, None, None), None);
    }

    #[test]
    fn serde_as_hex() {
        let v1: DhtKey = "0123456789abcdef0123456789abcdef01234567".parse().unwrap();
        let json = serde_json_like(&v1);
        assert_eq!(json, "0123456789abcdef0123456789abcdef01234567");
    }

    fn serde_json_like(k: &DhtKey) -> String {
        // Avoid a serde_json dev-dependency: Display is what Serialize uses.
        k.to_string()
    }

    #[test]
    fn shared_base_url_policy_accepts_strict_origins_only() {
        for ok in [
            "https://search.example.org",
            "http://127.0.0.1:8080",
            "https://example.com:8443",
            "http://example.com:8080",
            "http://my-host.example",
            "http://[::1]:8080",
            "https://[2001:db8::1]:8443",
        ] {
            assert!(validate_base_url(ok, MAX_BASE_URL_CHARS).is_ok(), "{ok}");
        }
        // Each of these passed one of the two previous per-crate validators:
        // uppercase scheme/host passed the web one, trailing slashes and
        // unterminated IPv6 literals passed it too, while the config one
        // already rejected all of them. The shared policy rejects them all.
        for bad in [
            "",
            "search.example.org",
            "ftp://example.org",
            "HTTPS://search.example.org",
            "https://Search.Example.org",
            "HTTPS://Search.Example.org",
            "https://",
            "https://search.example.org/",
            "https://example.org/search",
            "https://example.org?x",
            "https://user@example.org",
            "https://exa mple.org",
            "https://example.org\r\nX: y",
            "https://example.com:443",
            "http://example.com:80",
            "https://example.com:443/",
            "https://[::1]:443",
            "http://[::1]:80",
            "https://[::1",
            "https://example.com:0",
            "https://[::1]x",
        ] {
            assert!(
                validate_base_url(bad, MAX_BASE_URL_CHARS).is_err(),
                "{bad:?}"
            );
        }
        // The length gate counts characters against the caller's budget.
        assert!(validate_base_url("https://example.com", 5).is_err());
        assert!(validate_base_url("https://example.com", MAX_BASE_URL_CHARS).is_ok());
    }
}
