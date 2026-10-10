//! Request handlers and what they share.

pub(crate) mod api;
pub(crate) mod health;
pub(crate) mod pages;
pub(crate) mod search;
pub(crate) mod theme;
pub(crate) mod torrent;

use std::time::Duration;

use axum::extract::Path;
use axum::extract::rejection::PathRejection;
use chrono::{DateTime, Utc};
use dc4_core::{AnyKey, magnet_link, text};
use dc4_store::{FileRow, MAX_NAME_CHARS, MAX_PATH_CHARS, TorrentRecord};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::sync::SemaphorePermit;

use crate::app::{AppState, routes};
use crate::{Backend, BackendError, MAX_LISTED_PATH_CHARS};

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

/// The path of a torrent's page.
pub(crate) fn torrent_href(record: &TorrentRecord) -> String {
    format!("{}{}", routes::TORRENT_PREFIX, record.dht_key.to_hex())
}

/// The seeder estimate to show and rank on: `Some` only while the scrape
/// that produced it is still fresh (bep33.md §7). Stale or missing
/// estimates display and rank as if missing.
pub(crate) fn fresh_seeders(record: &TorrentRecord, freshness: Duration) -> Option<u64> {
    fresh_seeders_at(record, freshness, Utc::now())
}

/// [`fresh_seeders`] against an explicit clock reading, so a whole page can
/// rank against one shared `now` instead of sampling the clock per record or
/// per comparison.
pub(crate) fn fresh_seeders_at(
    record: &TorrentRecord,
    freshness: Duration,
    now: DateTime<Utc>,
) -> Option<u64> {
    let est = record.seeders_est?;
    let scraped = record.last_scraped_at?;
    let age = now.signed_duration_since(scraped);
    (age <= chrono::Duration::from_std(freshness).unwrap_or(chrono::Duration::MAX)).then_some(est)
}

/// A stored torrent's text, cleaned for display.
pub(crate) struct Shown {
    pub name: String,
    /// The file rows within [`MAX_LISTED_PATH_CHARS`], with cleaned paths
    /// (empty for result lists, which show no files).
    pub files: Vec<FileRow>,
    /// Files were left out to stay within [`MAX_LISTED_PATH_CHARS`].
    pub files_cut: bool,
    pub magnet: Option<String>,
}

/// Whether the file list shown for `record` is truncated. Call only for
/// [`Detail::WithFiles`]: result lists show no files by design, so their
/// empty `shown.files` must not count. The store may flag truncation, the
/// display budget may cut paths, or the store may hand back fewer files
/// than `file_count` without flagging either.
pub(crate) fn files_truncated(record: &TorrentRecord, shown: &Shown) -> bool {
    record.files_truncated
        || shown.files_cut
        || u64::try_from(shown.files.len()).unwrap_or(u64::MAX) < record.file_count
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
/// (R8).
///
/// Every listed path is prepared within the same [`MAX_LISTED_PATH_CHARS`]
/// budget it would be listed with. Result lists prepare the same file
/// paths they would list, so nothing displayed is unchecked.
pub(crate) fn show(record: &TorrentRecord, detail: Detail) -> Shown {
    let mut name = text::sanitize_display(&record.name, MAX_NAME_CHARS);
    if name.is_empty() {
        name = record.dht_key.to_hex();
    }
    let mut files = Vec::new();
    let mut files_cut = false;
    let mut budget = MAX_LISTED_PATH_CHARS;
    for file in &record.files {
        let path = text::sanitize_display(&file.path, MAX_PATH_CHARS);
        let Some(rest) = budget.checked_sub(path.chars().count()) else {
            // Past the budget paths are neither listed nor shown: on detail
            // pages the list is marked truncated, on result lists there is
            // nothing to list either way.
            files_cut = detail == Detail::WithFiles;
            break;
        };
        budget = rest;
        if detail == Detail::WithFiles {
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
    Shown {
        name,
        files,
        files_cut,
        magnet,
    }
}

/// Runs [`show`] for each record on the blocking pool, keeping the order
/// and dropping records that are no longer live. `None` if the task failed.
pub(crate) async fn show_all(
    records: Vec<TorrentRecord>,
    detail: Detail,
) -> Option<Vec<(TorrentRecord, Shown)>> {
    let task = tokio::task::spawn_blocking(move || {
        records
            .into_iter()
            .filter(TorrentRecord::is_live)
            .map(|record| {
                let shown = show(&record, detail);
                (record, shown)
            })
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
    match show_all(vec![record], Detail::WithFiles).await {
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
    use dc4_core::DhtKey;

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
            last_scraped_at: None,
            seeders_est: None,
            change_seq: 1,
            deleted_at: None,
        }
    }

    #[test]
    fn show_cleans_text() {
        let shown = show(
            &record("evil\u{202E}gpj.exe", &["a\u{0}b.txt"]),
            Detail::WithFiles,
        );
        assert_eq!(shown.name, "evilgpj.exe");
        assert_eq!(shown.files[0].path, "ab.txt");
        assert!(shown.magnet.unwrap().starts_with("magnet:?xt=urn:btih:"));
        // Every name is shown, whatever it contains.
        let shown = show(&record("A ForbiddenWord here", &[]), Detail::NameOnly);
        assert_eq!(shown.name, "A ForbiddenWord here");
        let shown = show(
            &record("fine", &["dir/forbiddenword.txt"]),
            Detail::WithFiles,
        );
        assert_eq!(shown.files[0].path, "dir/forbiddenword.txt");
        let unnamed = show(&record("\u{202E}", &[]), Detail::NameOnly);
        assert_eq!(unnamed.name, DhtKey([7; 20]).to_hex());
        // Result lists prepare file paths too, within the same budget.
        let shown = show(
            &record("fine", &["dir/forbiddenword.txt"]),
            Detail::NameOnly,
        );
        assert!(shown.files.is_empty());
        assert!(!shown.files_cut);
    }

    #[test]
    fn files_truncated_flags_all_three_causes() {
        // Store flag.
        let mut flagged = record("f", &["a"]);
        flagged.files_truncated = true;
        let shown = show(&flagged, Detail::WithFiles);
        assert!(files_truncated(&flagged, &shown));
        // Display budget cut.
        let long = "x".repeat(MAX_PATH_CHARS);
        let paths = vec![long.as_str(); 40];
        let cut_record = record("c", &paths);
        let cut = show(&cut_record, Detail::WithFiles);
        assert!(cut.files_cut);
        assert!(files_truncated(&cut_record, &cut));
        // Fewer files handed back than stored, with neither flag set.
        let mut short = record("s", &["a", "b"]);
        short.file_count = 5;
        let shown = show(&short, Detail::WithFiles);
        assert!(!short.files_truncated && !shown.files_cut);
        assert_eq!(shown.files.len(), 2);
        assert!(files_truncated(&short, &shown));
        // Complete listing: none of the three.
        let whole = record("w", &["a", "b"]);
        let shown = show(&whole, Detail::WithFiles);
        assert!(!files_truncated(&whole, &shown));
    }

    #[tokio::test]
    async fn show_all_keeps_order_and_drops_dead() {
        let mut dead = record("tombstoned", &[]);
        dead.deleted_at = Some(Utc::now());
        let records = vec![
            record("b first", &[]),
            dead,
            record("forbiddenword", &[]),
            record("a last", &[]),
        ];
        let shown = show_all(records, Detail::NameOnly).await.unwrap();
        let names: Vec<&str> = shown.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(names, ["b first", "forbiddenword", "a last"]);
    }

    #[test]
    fn show_limits_listed_path_text() {
        // Stored paths are at most MAX_PATH_CHARS long.
        let long = "x".repeat(MAX_PATH_CHARS);
        let paths = vec![long.as_str(); 40];
        let shown = show(&record("n", &paths), Detail::WithFiles);
        assert_eq!(shown.files.len(), MAX_LISTED_PATH_CHARS / MAX_PATH_CHARS);
        assert!(shown.files_cut);
        let few = show(&record("n", &paths[..3]), Detail::WithFiles);
        assert_eq!(few.files.len(), 3);
        assert!(!few.files_cut);
    }

    #[test]
    fn detail_pages_prepare_every_listed_path() {
        // Stored paths are at most MAX_PATH_CHARS long; enough of them
        // overflow the listing budget.
        let long = "x".repeat(MAX_PATH_CHARS);
        let paths = vec![long.as_str(); 40];
        let shown = show(&record("fine", &paths), Detail::WithFiles);
        assert_eq!(shown.files.len(), MAX_LISTED_PATH_CHARS / MAX_PATH_CHARS);
        assert!(shown.files_cut);
        // Result lists prepare nothing to list and are never truncated.
        let shown = show(&record("fine", &paths), Detail::NameOnly);
        assert!(shown.files.is_empty());
        assert!(!shown.files_cut);
        // Within the budget every path is listed.
        let shown = show(&record("fine", &paths[..3]), Detail::WithFiles);
        assert_eq!(shown.files.len(), 3);
        assert!(!shown.files_cut);
        // The name is always shown.
        let shown = show(&record("forbiddenword", &paths), Detail::WithFiles);
        assert_eq!(shown.name, "forbiddenword");
    }

    #[test]
    fn every_name_and_path_is_shown() {
        assert_eq!(
            show(&record("fine", &["xpthc/a.jpg"]), Detail::WithFiles)
                .files
                .len(),
            1
        );
        assert_eq!(
            show(&record("xpthc movie", &[]), Detail::NameOnly).name,
            "xpthc movie"
        );
        assert_eq!(
            show(&record("holiday photos", &["ok.jpg"]), Detail::NameOnly).name,
            "holiday photos"
        );
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
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(
            encode_query_value("a b&c=d#e/f?g+h%i\"<>'東"),
            "a%20b%26c%3Dd%23e%2Ff%3Fg%2Bh%25i%22%3C%3E%27%E6%9D%B1"
        );
        assert_eq!(encode_query_value("safe-._~"), "safe-._~");
    }

    fn scraped_record(est: Option<u64>, scraped_ago_secs: Option<i64>) -> TorrentRecord {
        let mut r = record("x", &[]);
        r.seeders_est = est;
        r.last_scraped_at = scraped_ago_secs.map(|s| Utc::now() - chrono::Duration::seconds(s));
        r
    }

    fn shown_of(r: &TorrentRecord) -> Shown {
        Shown {
            name: r.name.clone(),
            files: Vec::new(),
            files_cut: false,
            magnet: None,
        }
    }

    #[test]
    fn fresh_seeders_at_pins_the_boundary_against_a_fixed_clock() {
        use std::time::Duration;
        let freshness = Duration::from_secs(7 * 24 * 60 * 60);
        let window = chrono::Duration::from_std(freshness).unwrap();
        let now = Utc::now();
        let scraped_at = |age: chrono::Duration| {
            let mut r = record("x", &[]);
            r.seeders_est = Some(7);
            r.last_scraped_at = Some(now - age);
            r
        };
        // Exactly at the window edge still counts as fresh …
        assert_eq!(
            fresh_seeders_at(&scraped_at(window), freshness, now),
            Some(7)
        );
        // … one second past it does not.
        assert_eq!(
            fresh_seeders_at(
                &scraped_at(window + chrono::Duration::seconds(1)),
                freshness,
                now
            ),
            None
        );
        // A scrape timestamped after `now` (clock skew) counts as fresh.
        let mut skewed = record("x", &[]);
        skewed.seeders_est = Some(7);
        skewed.last_scraped_at = Some(now + chrono::Duration::seconds(1));
        assert_eq!(fresh_seeders_at(&skewed, freshness, now), Some(7));
        // The shared reading decides: the same record is fresh and stale
        // against different `now`s with no clock sampled in between.
        let r = scraped_at(window);
        assert_eq!(fresh_seeders_at(&r, freshness, now), Some(7));
        assert_eq!(
            fresh_seeders_at(&r, freshness, now + chrono::Duration::seconds(2)),
            None
        );
    }

    #[test]
    fn fresh_seeders_hides_missing_and_stale_estimates() {
        use std::time::Duration;
        let freshness = Duration::from_secs(7 * 24 * 60 * 60);
        assert_eq!(fresh_seeders(&scraped_record(None, None), freshness), None);
        assert_eq!(
            fresh_seeders(&scraped_record(Some(5), None), freshness),
            None
        );
        assert_eq!(
            fresh_seeders(&scraped_record(Some(5), Some(100)), freshness),
            Some(5)
        );
        // Older than the window: hidden (stale).
        assert_eq!(
            fresh_seeders(&scraped_record(Some(5), Some(8 * 24 * 60 * 60)), freshness),
            None
        );
        // Zero is a measured value, shown like any other.
        assert_eq!(
            fresh_seeders(&scraped_record(Some(0), Some(100)), freshness),
            Some(0)
        );
    }

    #[test]
    fn seeder_ordering_sorts_estimates_first() {
        use std::collections::HashMap;
        use std::time::Duration;

        use super::search::order_page;
        let freshness = Duration::from_secs(7 * 24 * 60 * 60);
        let mut page: Vec<(TorrentRecord, Shown)> = [None, Some(0), Some(30), Some(5)]
            .into_iter()
            .map(|est| {
                let r = scraped_record(est, est.map(|_| 100));
                let s = shown_of(&r);
                (r, s)
            })
            .collect();
        order_page(
            &mut page,
            &HashMap::new(),
            dc4_search::Sort::Seeders,
            freshness,
        );
        let ests: Vec<Option<u64>> = page.iter().map(|(r, _)| r.seeders_est).collect();
        assert_eq!(ests, [Some(30), Some(5), Some(0), None]);

        // Other sorts leave the page alone.
        let mut page: Vec<(TorrentRecord, Shown)> = [Some(30), Some(5)]
            .into_iter()
            .map(|est| {
                let r = scraped_record(est, est.map(|_| 100));
                let s = shown_of(&r);
                (r, s)
            })
            .collect();
        order_page(
            &mut page,
            &HashMap::new(),
            dc4_search::Sort::Seen,
            freshness,
        );
        let ests: Vec<Option<u64>> = page.iter().map(|(r, _)| r.seeders_est).collect();
        assert_eq!(ests, [Some(30), Some(5)]);
    }

    #[test]
    fn relevance_boost_prefers_fresh_seeders() {
        use std::collections::HashMap;
        use std::time::Duration;

        use super::search::{boosted_at, order_page};
        let freshness = Duration::from_secs(7 * 24 * 60 * 60);
        // A fresh estimate multiplies the score; stale and missing do not.
        let fresh = scraped_record(Some(99), Some(100));
        let stale = scraped_record(Some(9999), Some(8 * 24 * 60 * 60));
        let missing = scraped_record(None, None);
        let base = 2.0f32;
        assert!(boosted_at(&fresh, Some(&base), freshness, chrono::Utc::now()) > base);
        assert_eq!(
            boosted_at(&stale, Some(&base), freshness, chrono::Utc::now()),
            base
        );
        assert_eq!(
            boosted_at(&missing, Some(&base), freshness, chrono::Utc::now()),
            base
        );
        assert_eq!(boosted_at(&fresh, None, freshness, chrono::Utc::now()), 0.0);

        // The boost re-ranks the page: the lower BM25 score with many
        // fresh seeders comes first; a stale swarm does not move.
        let mk = [missing, fresh, stale]
            .into_iter()
            .enumerate()
            .map(|(i, mut r)| {
                r.id = i as i64 + 1;
                (r.clone(), shown_of(&r))
            });
        let mut page: Vec<(TorrentRecord, Shown)> = mk.collect();
        // Fresh (id 2) is boosted past the leader; stale (id 3) is not.
        let scores: HashMap<i64, f32> = [(1, 3.0), (2, 2.9), (3, 2.95)].into_iter().collect();
        order_page(&mut page, &scores, dc4_search::Sort::Relevance, freshness);
        let ids: Vec<i64> = page.iter().map(|(r, _)| r.id).collect();
        // Fresh (id 2, boosted past 3.0) first; stale (id 3) and missing
        // (id 1) keep index order.
        assert_eq!(ids, [2, 1, 3]);
    }
}
