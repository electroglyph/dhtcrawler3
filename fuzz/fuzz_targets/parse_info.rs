#![no_main]
//! Any byte string must parse to torrent metadata or an error, never a panic.
//! Parsed metadata must respect the documented caps, and `verify` must accept
//! the input under its own SHA-1.

use dc3_core::DhtKey;
use dc3_torrent::{
    MAX_FILES_PARSED, MAX_FILES_STORED, NAME_MAX_CHARS, PATH_MAX_CHARS, UNNAMED, Verified,
    parse_info_visit, verify,
};
use libfuzzer_sys::fuzz_target;
use sha1::{Digest, Sha1};

fuzz_target!(|data: &[u8]| {
    let mut calls = 0usize;
    let mut first: Option<String> = None;
    let mut visit = |s: &str| {
        if calls == 0 {
            first = Some(s.to_owned());
        } else {
            assert!(s.chars().count() <= PATH_MAX_CHARS, "path too long");
            assert!(!s.is_empty(), "empty path visited");
        }
        calls += 1;
    };
    let parsed = parse_info_visit(data, &mut visit);
    assert!(calls <= MAX_FILES_PARSED + 1);
    if let Ok(meta) = parsed {
        assert!(calls >= 1, "name was not visited");
        assert_eq!(first.as_deref(), Some(meta.name.as_str()));
        assert!(!meta.name.is_empty());
        assert!(meta.name == UNNAMED || meta.name.chars().count() <= NAME_MAX_CHARS);
        assert!(meta.files.len() <= MAX_FILES_STORED);
        assert!(meta.files.len() as u64 <= meta.file_count);
        assert!(meta.files.is_sorted());
        assert!(!meta.files_truncated || meta.files.len() == MAX_FILES_STORED);
        let listed: u64 = meta.files.iter().map(|f| f.size).sum();
        assert!(listed <= meta.total_size);
        for f in &meta.files {
            assert!(f.path.chars().count() <= PATH_MAX_CHARS);
            assert!(!f.path.starts_with('/'));
        }
        assert!(meta.info_hash_v1.is_some() || meta.info_hash_v2.is_some());
        if let Some(v1) = meta.info_hash_v1 {
            assert_eq!(verify(&v1, data), Some(Verified::V1));
        }
        if let Some(v2) = meta.info_hash_v2 {
            assert!(verify(&v2.truncated(), data).is_some());
        }
        assert!(meta.piece_length.is_none_or(|p| p > 0));
    }

    let own = DhtKey(Sha1::digest(data).into());
    assert_eq!(verify(&own, data), Some(Verified::V1));

    // A key taken from the input itself almost never matches the rest.
    let (head, rest) = data.split_at(data.len().min(DhtKey::LEN));
    let mut key = [0u8; DhtKey::LEN];
    key[..head.len()].copy_from_slice(head);
    let _ = verify(&DhtKey(key), rest);
});
