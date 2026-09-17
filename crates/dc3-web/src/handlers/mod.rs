//! Request handlers and what they share.

pub(crate) mod api;
pub(crate) mod health;
pub(crate) mod pages;
pub(crate) mod report;
pub(crate) mod search;
pub(crate) mod torrent;

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::rejection::PathRejection;
use dc3_core::{AnyKey, magnet_link, text};
use dc3_policy::TermMatcher;
use dc3_store::{FileRow, MAX_NAME_CHARS, MAX_PATH_CHARS, TorrentRecord};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::sync::SemaphorePermit;

use crate::app::{AppState, routes};
use crate::{Backend, BackendError, MAX_CHECKED_PATH_BYTES, MAX_LISTED_PATH_CHARS};

/// Characters left unescaped in a query-string value (RFC 3986 unreserved).
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Longest key text accepted in a path (a v2 infohash in hex).
const MAX_KEY_CHARS: usize = 64;

/// Percent-encodes a query-string value.
pub(crate) fn encode_query_value(value: &str) -> String {
    utf8_percent_encode(value, QUERY_VALUE).to_string()
}

/// The key named by a `{key}` path segment: 40 hex or 32 base32 characters
/// for a DHT or v1 key, 64 hex characters for a v2 key.
pub(crate) fn parse_key(segment: Result<Path<String>, PathRejection>) -> Option<AnyKey> {
    let Path(text) = segment.ok()?;
    if text.len() > MAX_KEY_CHARS {
        return None;
    }
    text.parse().ok()
}

/// A key in its canonical lowercase hex form.
pub(crate) fn key_hex(key: &AnyKey) -> String {
    match key {
        AnyKey::V1OrDht(k) => k.to_hex(),
        AnyKey::V2(h) => h.to_hex(),
    }
}

/// The path of a torrent's page.
pub(crate) fn torrent_href(record: &TorrentRecord) -> String {
    format!("{}{}", routes::TORRENT_PREFIX, record.dht_key.to_hex())
}

/// A stored torrent's text, cleaned for display and checked against the
/// blocked terms.
pub(crate) struct Shown {
    pub name: String,
    /// The file rows within [`MAX_LISTED_PATH_CHARS`], with cleaned paths
    /// (empty for result lists, which show no files).
    pub files: Vec<FileRow>,
    /// Files were left out to stay within [`MAX_LISTED_PATH_CHARS`].
    pub files_cut: bool,
    pub magnet: Option<String>,
}

/// What of a stored torrent is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    /// A result list: the name only.
    NameOnly,
    /// A detail page: the name and the file list.
    WithFiles,
}

/// Cleans a stored torrent's name (and, for [`Detail::WithFiles`], its
/// listed paths) again before display, because they come from strangers
/// (R8). Returns `None` if any displayed text contains a blocked term (R18);
/// the torrent is then not shown at all.
///
/// Matching can cost several microseconds per character (non-ASCII text is
/// the slow case), so result lists check only names, and detail pages
/// check paths only within the first [`MAX_CHECKED_PATH_BYTES`]. The
/// indexer checked every stored path, and deletes matches after a policy
/// change.
pub(crate) fn show(record: &TorrentRecord, policy: &TermMatcher, detail: Detail) -> Option<Shown> {
    let mut name = text::sanitize_display(&record.name, MAX_NAME_CHARS);
    if name.is_empty() {
        name = record.dht_key.to_hex();
    }
    if policy.matches(&name) {
        return None;
    }
    let mut files = Vec::new();
    let mut files_cut = false;
    if detail == Detail::WithFiles {
        let mut budget = MAX_LISTED_PATH_CHARS;
        let mut check_budget = MAX_CHECKED_PATH_BYTES;
        for file in &record.files {
            let path = text::sanitize_display(&file.path, MAX_PATH_CHARS);
            let Some(rest) = budget.checked_sub(path.chars().count()) else {
                files_cut = true;
                break;
            };
            budget = rest;
            match check_budget.checked_sub(path.len()) {
                Some(rest) => {
                    check_budget = rest;
                    if policy.matches(&path) {
                        return None;
                    }
                }
                None => check_budget = 0,
            }
            files.push(FileRow {
                path,
                size: file.size,
            });
        }
    }
    let magnet = magnet_link(
        record.info_hash_v1.as_ref(),
        record.info_hash_v2.as_ref(),
        Some(&name),
    );
    Some(Shown {
        name,
        files,
        files_cut,
        magnet,
    })
}

/// Runs [`show`] for each record on the blocking pool, keeping the order
/// and dropping records that must not be shown. `None` if the task failed.
pub(crate) async fn show_all(
    records: Vec<TorrentRecord>,
    policy: &Arc<TermMatcher>,
    detail: Detail,
) -> Option<Vec<(TorrentRecord, Shown)>> {
    let policy = Arc::clone(policy);
    let task = tokio::task::spawn_blocking(move || {
        records
            .into_iter()
            .filter(TorrentRecord::is_live)
            .filter_map(|record| show(&record, &policy, detail).map(|shown| (record, shown)))
            .collect()
    });
    task.await.ok()
}

/// The outcome of a torrent-detail lookup.
pub(crate) enum Lookup<'a> {
    Found {
        record: Box<TorrentRecord>,
        shown: Shown,
        /// Keep this while building the response, to bound memory use.
        permit: SemaphorePermit<'a>,
    },
    Missing,
    Unavailable,
}

/// Looks up a visible torrent for a detail page or API response, at most
/// [`crate::MAX_CONCURRENT_DETAILS`] at a time.
pub(crate) async fn lookup<B: Backend>(st: &AppState<B>, key: AnyKey) -> Lookup<'_> {
    // The semaphore is never closed.
    let Ok(permit) = st.details.acquire().await else {
        return Lookup::Unavailable;
    };
    let record = match st.backend.get_by_key(key).await {
        Ok(Some(record)) => record,
        Ok(None) => return Lookup::Missing,
        Err(e) => {
            log_backend_error(&e, "torrent lookup");
            return Lookup::Unavailable;
        }
    };
    match show_all(vec![record], &st.policy, Detail::WithFiles).await {
        Some(mut shown) => match shown.pop() {
            Some((record, shown)) => Lookup::Found {
                record: Box::new(record),
                shown,
                permit,
            },
            None => Lookup::Missing,
        },
        None => Lookup::Unavailable,
    }
}

/// Logs a backend failure. The error never contains visitor data.
pub(crate) fn log_backend_error(e: &BackendError, operation: &'static str) {
    tracing::warn!(error = %e, operation, "database request failed");
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use dc3_core::DhtKey;

    use super::*;

    fn record(name: &str, paths: &[&str]) -> TorrentRecord {
        let key = DhtKey([7; 20]);
        TorrentRecord {
            id: 1,
            dht_key: key,
            info_hash_v1: Some(key),
            info_hash_v2: None,
            name: name.to_owned(),
            total_size: 10,
            file_count: u64::try_from(paths.len()).unwrap(),
            files: paths
                .iter()
                .map(|p| FileRow {
                    path: (*p).to_owned(),
                    size: 5,
                })
                .collect(),
            files_truncated: false,
            piece_length: None,
            seen_count: 1,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            change_seq: 1,
            hidden_at: None,
            reviewed_at: None,
            deleted_at: None,
        }
    }

    #[test]
    fn show_cleans_text_and_applies_the_policy() {
        let policy = TermMatcher::load("forbiddenword\n").unwrap();
        let shown = show(
            &record("evil\u{202E}gpj.exe", &["a\u{0}b.txt"]),
            &policy,
            Detail::WithFiles,
        )
        .unwrap();
        assert_eq!(shown.name, "evilgpj.exe");
        assert_eq!(shown.files[0].path, "ab.txt");
        assert!(shown.magnet.unwrap().starts_with("magnet:?xt=urn:btih:"));
        assert!(
            show(
                &record("A ForbiddenWord here", &[]),
                &policy,
                Detail::NameOnly
            )
            .is_none()
        );
        assert!(
            show(
                &record("fine", &["dir/forbiddenword.txt"]),
                &policy,
                Detail::WithFiles
            )
            .is_none()
        );
        let unnamed = show(&record("\u{202E}", &[]), &policy, Detail::NameOnly).unwrap();
        assert_eq!(unnamed.name, DhtKey([7; 20]).to_hex());
        // Result lists neither list nor check paths.
        let listed = show(
            &record("fine", &["dir/forbiddenword.txt"]),
            &policy,
            Detail::NameOnly,
        )
        .unwrap();
        assert!(listed.files.is_empty());
        assert!(!listed.files_cut);
    }

    #[tokio::test]
    async fn show_all_keeps_order_and_drops_hidden_and_blocked() {
        let policy = Arc::new(TermMatcher::load("forbiddenword\n").unwrap());
        let mut hidden = record("hidden", &[]);
        hidden.hidden_at = Some(Utc::now());
        let records = vec![
            record("b first", &[]),
            hidden,
            record("forbiddenword", &[]),
            record("a last", &[]),
        ];
        let shown = show_all(records, &policy, Detail::NameOnly).await.unwrap();
        let names: Vec<&str> = shown.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(names, ["b first", "a last"]);
    }

    #[test]
    fn show_limits_listed_path_text() {
        // Stored paths are at most MAX_PATH_CHARS long.
        let long = "x".repeat(MAX_PATH_CHARS);
        let paths = vec![long.as_str(); 40];
        let shown = show(
            &record("n", &paths),
            &TermMatcher::empty(),
            Detail::WithFiles,
        )
        .unwrap();
        assert_eq!(shown.files.len(), MAX_LISTED_PATH_CHARS / MAX_PATH_CHARS);
        assert!(shown.files_cut);
        let few = show(
            &record("n", &paths[..3]),
            &TermMatcher::empty(),
            Detail::WithFiles,
        )
        .unwrap();
        assert_eq!(few.files.len(), 3);
        assert!(!few.files_cut);
    }

    #[test]
    fn detail_pages_match_a_bounded_amount_of_path_text() {
        let policy = TermMatcher::load("forbiddenword\n").unwrap();
        // 3000 bytes of a character the matcher is slow on.
        let filler = "\u{FDFA}".repeat(1000);
        let late = "dir/forbiddenword.txt";
        // Past 8 KiB of paths, paths are listed but not matched again (the
        // indexer checked every stored path).
        let paths = [filler.as_str(), filler.as_str(), filler.as_str(), late];
        let shown = show(&record("fine", &paths), &policy, Detail::WithFiles).unwrap();
        assert_eq!(shown.files.len(), 4);
        assert!(!shown.files_cut);
        // Within the budget a path is still matched.
        let early = [filler.as_str(), late];
        assert!(show(&record("fine", &early), &policy, Detail::WithFiles).is_none());
        // The name is always matched.
        assert!(show(&record("forbiddenword", &paths), &policy, Detail::WithFiles).is_none());
    }

    #[test]
    fn keys() {
        let ok = |s: &str| parse_key(Ok(Path(s.to_owned())));
        assert!(ok(&"a".repeat(40)).is_some());
        assert!(ok(&"A".repeat(40)).is_some());
        assert!(ok(&"b".repeat(64)).is_some());
        assert!(ok(&"a".repeat(32)).is_some());
        assert!(ok("abc").is_none());
        assert!(ok(&"g".repeat(40)).is_none());
        assert!(ok(&"a".repeat(65)).is_none());
        let v2: AnyKey = "c".repeat(64).parse().unwrap();
        assert_eq!(key_hex(&v2), "c".repeat(64));
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(
            encode_query_value("a b&c=d#e/f?g+h%i\"<>'東"),
            "a%20b%26c%3Dd%23e%2Ff%3Fg%2Bh%25i%22%3C%3E%27%E6%9D%B1"
        );
        assert_eq!(encode_query_value("safe-._~"), "safe-._~");
    }
}
