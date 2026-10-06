//! BitTorrent info-dictionary parsing and metadata verification (BEP 3, 9, 47, 52).
//!
//! Metadata fetched from a peer is attacker-controlled (axiom A2 in
//! `docs/01-first-principles.md`). The crawler must therefore:
//!
//! 1. [`verify`] the bytes against the DHT key they were fetched for (R6),
//!    and only then
//! 2. [`parse_info`] them into a bounded, sanitised [`TorrentMeta`] (R4, R8),
//!    or [`parse_info_visit`] to also see every sanitised name and path.
//!
//! Parsing never panics, is bounded by the constants below, and is
//! deterministic: the same bytes always produce the same result.
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]

mod parse;
mod text;

#[cfg(test)]
mod tests;

use dc3_core::{DhtKey, InfoHashV2};
use sha1::{Digest, Sha1};
use sha2::Sha256;

pub use parse::{parse_info, parse_info_visit};

/// Maximum characters of a torrent name after sanitising.
pub const NAME_MAX_CHARS: usize = 1024;
/// Maximum characters of a file path (components joined with `/`) after sanitising.
pub const PATH_MAX_CHARS: usize = 4096;
/// Maximum file entries (including padding files) parsed from one info dictionary.
pub const MAX_FILES_PARSED: usize = 200_000;
/// Maximum file entries kept in [`TorrentMeta::files`].
pub const MAX_FILES_STORED: usize = 2_000;
/// Maximum directory nesting of a v2 `file tree` (path components per file).
pub const MAX_FILE_TREE_DEPTH: usize = 64;
/// Characters of text [`parse_info_visit`] may show per byte of metadata.
pub const TEXT_CHARS_PER_BYTE: usize = 4;
/// Lower bound of the visited-text budget, so small torrents with deep trees parse.
pub const MIN_TEXT_CHARS: usize = 1 << 20;
/// Upper bound of the visited-text budget input (per byte of metadata,
/// clamped); total visited text is bounded by budget + metadata length and
/// parsing fails with [`ParseError::TooMuchText`] past the budget.
pub const MAX_TEXT_CHARS: usize = 16 << 20;
/// Name used when a torrent's name is missing or sanitises to nothing.
pub const UNNAMED: &str = "(unnamed)";

/// How metadata matched the DHT key it was fetched for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verified {
    /// SHA-1 of the info dictionary equals the key (v1 or hybrid torrent).
    V1,
    /// The first 20 bytes of SHA-256 of the info dictionary equal the key (v2 torrent).
    V2Truncated,
}

/// Checks that `info` is the metadata for `key`: SHA-1 equals the key, or
/// else truncated SHA-256 equals the key. `None` means it matches neither.
pub fn verify(key: &DhtKey, info: &[u8]) -> Option<Verified> {
    if sha1_key(info) == *key {
        return Some(Verified::V1);
    }
    if sha256_hash(info).truncated() == *key {
        return Some(Verified::V2Truncated);
    }
    None
}

pub(crate) fn sha1_key(info: &[u8]) -> DhtKey {
    DhtKey(Sha1::digest(info).into())
}

pub(crate) fn sha256_hash(info: &[u8]) -> InfoHashV2 {
    InfoHashV2(Sha256::digest(info).into())
}

/// The searchable description of a torrent, taken from its info dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentMeta {
    /// Sanitised name; [`UNNAMED`] if empty.
    pub name: String,
    /// SHA-1 of the info dictionary, present when it has v1 `pieces`.
    pub info_hash_v1: Option<DhtKey>,
    /// SHA-256 of the info dictionary, present when `meta version` is 2 with a `file tree`.
    pub info_hash_v2: Option<InfoHashV2>,
    /// Sum of all non-padding file sizes, including files omitted from `files`.
    pub total_size: u64,
    /// Number of non-padding files, including files omitted from `files`.
    pub file_count: u64,
    /// At most [`MAX_FILES_STORED`] non-padding files, sorted by path then size.
    pub files: Vec<FileEntry>,
    /// True when more listable files existed than [`MAX_FILES_STORED`].
    pub files_truncated: bool,
    /// `piece length`, when present and positive.
    pub piece_length: Option<u64>,
    /// `private` == 1 (BEP 27).
    pub private: bool,
}

/// One file of a torrent. `path` is relative, uses `/` and has sanitised components.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
}

/// Why an info dictionary was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("invalid bencode: {0}")]
    Bencode(#[from] dc3_bencode::Error),
    #[error("info is not a dictionary")]
    NotADict,
    #[error("neither v1 `pieces` nor a v2 `file tree` is present")]
    NotATorrent,
    #[error("missing or invalid field `{0}`")]
    InvalidField(&'static str),
    #[error("negative file length")]
    NegativeLength,
    #[error("total size overflows")]
    SizeOverflow,
    #[error("more than {MAX_FILES_PARSED} files")]
    TooManyFiles,
    #[error("file tree deeper than {MAX_FILE_TREE_DEPTH}")]
    FileTreeTooDeep,
    #[error("joined paths exceed the text budget")]
    TooMuchText,
    #[error("malformed file tree: {0}")]
    InvalidFileTree(&'static str),
}
