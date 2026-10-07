//! Data types passed to and returned from [`crate::Store`].

use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use dc3_core::{DhtKey, InfoHashV2};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;

use crate::{Result, StoreError, get, to_u64};

/// One stored file entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRow {
    pub path: String,
    pub size: u64,
}

/// The JSON form of a [`FileRow`] inside `torrents.files`.
#[derive(Serialize, Deserialize)]
pub(crate) struct FileJson<'a> {
    #[serde(borrow)]
    p: std::borrow::Cow<'a, str>,
    s: u64,
}

pub(crate) fn files_to_json(files: &[FileRow]) -> Result<serde_json::Value> {
    let rows: Vec<FileJson<'_>> = files
        .iter()
        .map(|f| FileJson {
            p: std::borrow::Cow::Borrowed(f.path.as_str()),
            s: f.size,
        })
        .collect();
    serde_json::to_value(rows).map_err(|e| StoreError::Invalid(format!("files: {e}")))
}

pub(crate) fn files_from_json(v: serde_json::Value) -> Result<Vec<FileRow>> {
    let rows: Vec<FileJsonOwned> = serde_json::from_value(v)
        .map_err(|e| StoreError::Corrupt(format!("torrents.files: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| FileRow {
            path: r.p,
            size: r.s,
        })
        .collect())
}

#[derive(Deserialize)]
struct FileJsonOwned {
    p: String,
    s: u64,
}

/// A torrent to store after a successful, verified fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTorrent {
    pub dht_key: DhtKey,
    pub info_hash_v1: Option<DhtKey>,
    pub info_hash_v2: Option<InfoHashV2>,
    pub name: String,
    pub total_size: u64,
    pub file_count: u64,
    /// At most [`crate::MAX_STORED_FILES`] rows.
    pub files: Vec<FileRow>,
    pub files_truncated: bool,
    pub piece_length: Option<u64>,
}

/// Every column of a `torrents` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TorrentRecord {
    pub id: i64,
    pub dht_key: DhtKey,
    pub info_hash_v1: Option<DhtKey>,
    pub info_hash_v2: Option<InfoHashV2>,
    pub name: String,
    pub total_size: u64,
    pub file_count: u64,
    pub files: Vec<FileRow>,
    pub files_truncated: bool,
    pub piece_length: Option<u64>,
    pub seen_count: u64,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    /// Last BEP 33 scrape (`None` until the first scrape); also the claim
    /// lease, so it can be newer than the estimate it guards.
    pub last_scraped_at: Option<DateTime<Utc>>,
    /// Estimated seeders from the last aware scrape (`None` until one).
    /// Displayed only while fresh (see the web role); never indexed.
    pub seeders_est: Option<u64>,
    pub change_seq: i64,
    /// Tombstoned.
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Every column of [`TorrentRecord::from_row`] except `files`, for table
/// alias `t`.
macro_rules! torrent_columns_base {
    () => {
        "t.id, t.dht_key, t.info_hash_v1, t.info_hash_v2, t.name, t.total_size, t.file_count, \
         t.files_truncated, t.piece_length, t.seen_count, t.first_seen_at, \
         t.last_seen_at, t.last_scraped_at, t.seeders_est, t.change_seq, \
         t.deleted_at"
    };
}
pub(crate) use torrent_columns_base;

/// SQL expression: the row (alias `t`) may be shown and indexed.
macro_rules! visible_sql {
    () => {
        "(t.deleted_at IS NULL)"
    };
}
pub(crate) use visible_sql;

pub(crate) fn dht_key_col(row: &PgRow, col: &str) -> Result<DhtKey> {
    let b: Vec<u8> = get(row, col)?;
    DhtKey::from_slice(&b).map_err(|e| StoreError::Corrupt(format!("{col}: {e}")))
}

pub(crate) fn opt_dht_key_col(row: &PgRow, col: &str) -> Result<Option<DhtKey>> {
    let b: Option<Vec<u8>> = get(row, col)?;
    b.map(|b| DhtKey::from_slice(&b).map_err(|e| StoreError::Corrupt(format!("{col}: {e}"))))
        .transpose()
}

pub(crate) fn opt_v2_col(row: &PgRow, col: &str) -> Result<Option<InfoHashV2>> {
    let b: Option<Vec<u8>> = get(row, col)?;
    b.map(|b| InfoHashV2::from_slice(&b).map_err(|e| StoreError::Corrupt(format!("{col}: {e}"))))
        .transpose()
}

fn opt_u64(what: &str, v: Option<i64>) -> Result<Option<u64>> {
    v.map(|v| to_u64(what, v)).transpose()
}

impl TorrentRecord {
    pub(crate) fn from_row(row: &PgRow) -> Result<TorrentRecord> {
        Ok(TorrentRecord {
            id: get(row, "id")?,
            dht_key: dht_key_col(row, "dht_key")?,
            info_hash_v1: opt_dht_key_col(row, "info_hash_v1")?,
            info_hash_v2: opt_v2_col(row, "info_hash_v2")?,
            name: get(row, "name")?,
            total_size: to_u64("total_size", get(row, "total_size")?)?,
            file_count: to_u64("file_count", get(row, "file_count")?)?,
            files: files_from_json(get(row, "files")?)?,
            files_truncated: get(row, "files_truncated")?,
            piece_length: opt_u64("piece_length", get(row, "piece_length")?)?,
            seen_count: to_u64("seen_count", get(row, "seen_count")?)?,
            first_seen_at: get(row, "first_seen_at")?,
            last_seen_at: get(row, "last_seen_at")?,
            last_scraped_at: get(row, "last_scraped_at")?,
            seeders_est: {
                let est: Option<i32> = get(row, "seeders_est")?;
                est.map(|e| to_u64("seeders_est", i64::from(e)))
                    .transpose()?
            },
            change_seq: get(row, "change_seq")?,
            deleted_at: get(row, "deleted_at")?,
        })
    }

    /// True when the row is not tombstoned.
    pub fn is_live(&self) -> bool {
        self.deleted_at.is_none()
    }
}

/// A leased queue item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingItem {
    pub dht_key: DhtKey,
    /// Failed attempts so far.
    pub attempts: u32,
    pub seen_count: u64,
}

/// A stored torrent claimed for a BEP 33 scrape (see
/// [`crate::Store::claim_scrape_due`]). The snapshot of `last_seen_at` and
/// `change_seq` is what [`crate::Store::tombstone_dead`] checks before
/// wiping the row, so a concurrent fetch is never clobbered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrapeItem {
    pub id: i64,
    pub dht_key: DhtKey,
    /// Last aware estimate, if any; `None` means never scraped or only
    /// unaware scrapes so far.
    pub seeders_est: Option<u32>,
    /// Consecutive dead scrapes.
    pub scrape_failures: u32,
    pub last_seen_at: DateTime<Utc>,
    pub change_seq: i64,
}

impl ScrapeItem {
    pub(crate) fn from_row(row: &PgRow) -> Result<ScrapeItem> {
        let est: Option<i32> = get(row, "seeders_est")?;
        let failures: i32 = get(row, "scrape_failures")?;
        Ok(ScrapeItem {
            id: get(row, "id")?,
            dht_key: dht_key_col(row, "dht_key")?,
            seeders_est: est
                .map(|e| {
                    u32::try_from(e)
                        .map_err(|_| StoreError::Corrupt(format!("negative seeders_est: {e}")))
                })
                .transpose()?,
            scrape_failures: u32::try_from(failures).map_err(|_| {
                StoreError::Corrupt(format!("negative scrape_failures: {failures}"))
            })?,
            last_seen_at: get(row, "last_seen_at")?,
            change_seq: get(row, "change_seq")?,
        })
    }
}

/// The admission cooldown of one removed key, as returned by
/// [`crate::Store::removal_cooldowns`]. `remaining` already reflects the
/// ÷4 strong-evidence shortening (enough post-removal sightings, or a
/// seed announce): positive while the key stays out of admission, zero
/// once it may be re-admitted. Shortening never bypasses: a fresh
/// removal still blocks, just for a quarter of the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemovalCooldown {
    pub key: DhtKey,
    pub remaining: Duration,
    /// Post-removal sightings counted so far.
    pub sightings: u32,
}

/// One key seen by the crawler, for [`crate::Store::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub key: DhtKey,
    /// Times the key was seen since the last batch; 0 is ignored.
    pub sightings: u32,
    /// Queued even when the queue is full (keys from BEP 51 samples and
    /// announces).
    pub priority: bool,
}

/// Result of [`crate::Store::observe`], counted in distinct keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ObserveOutcome {
    /// Keys that matched a stored torrent.
    pub known: u64,
    /// Keys newly inserted into `pending`. Keys already pending are counted in
    /// none of the fields.
    pub queued: u64,
    /// New keys not queued because the queue was full and they were not
    /// priority keys.
    pub dropped: u64,
}

/// Pipeline counters for one UTC day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DailyStats {
    pub day: NaiveDate,
    pub discovered: u64,
    pub fetched: u64,
    pub fetch_failed: u64,
}

impl DailyStats {
    pub(crate) fn from_row(row: &PgRow) -> Result<DailyStats> {
        Ok(DailyStats {
            day: get(row, "day")?,
            discovered: to_u64("discovered", get(row, "discovered")?)?,
            fetched: to_u64("fetched", get(row, "fetched")?)?,
            fetch_failed: to_u64("fetch_failed", get(row, "fetch_failed")?)?,
        })
    }
}

/// Totals for the `stats` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct StoreStats {
    /// Torrents neither hidden nor tombstoned (exact up to
    /// [`crate::EXACT_COUNT_THRESHOLD`]; above it, the planner's estimate of
    /// all rows).
    pub torrents: u64,
    /// Queue rows that have not given up.
    pub pending: u64,
    pub gave_up: u64,
}

/// Totals for the public home page, readable by the web user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PublicStats {
    /// As [`StoreStats::torrents`].
    pub torrents: i64,
    /// `stats_daily.fetched` for today (UTC).
    pub added_today: i64,
    /// `stats_daily.fetched` for yesterday (UTC).
    pub added_yesterday: i64,
}

/// One row of the change feed for the indexer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexRow {
    pub id: i64,
    pub change_seq: i64,
    pub dht_key: DhtKey,
    pub info_hash_v1: Option<DhtKey>,
    pub info_hash_v2: Option<InfoHashV2>,
    pub name: String,
    /// File paths joined by `'\n'`.
    pub files_text: String,
    pub total_size: u64,
    pub file_count: u64,
    pub first_seen_at: DateTime<Utc>,
    pub seen_count: u64,
    /// Last BEP 33 scrape, if any. Carried for readers that join the feed
    /// (the indexer itself never scores on it: scrape writes must not bump
    /// `change_seq`, and reindexing per scrape would churn the index).
    pub last_scraped_at: Option<DateTime<Utc>>,
    /// Estimated seeders from the last aware scrape, if any. Same caveat.
    pub seeders_est: Option<u64>,
    /// False when the row is tombstoned; the indexer must then delete the
    /// document.
    pub visible: bool,
}

impl IndexRow {
    pub(crate) fn from_row(row: &PgRow) -> Result<IndexRow> {
        Ok(IndexRow {
            id: get(row, "id")?,
            change_seq: get(row, "change_seq")?,
            dht_key: dht_key_col(row, "dht_key")?,
            info_hash_v1: opt_dht_key_col(row, "info_hash_v1")?,
            info_hash_v2: opt_v2_col(row, "info_hash_v2")?,
            name: get(row, "name")?,
            files_text: get(row, "files_text")?,
            total_size: to_u64("total_size", get(row, "total_size")?)?,
            file_count: to_u64("file_count", get(row, "file_count")?)?,
            first_seen_at: get(row, "first_seen_at")?,
            seen_count: to_u64("seen_count", get(row, "seen_count")?)?,
            last_scraped_at: get(row, "last_scraped_at")?,
            seeders_est: {
                let est: Option<i32> = get(row, "seeders_est")?;
                est.map(|e| to_u64("seeders_est", i64::from(e)))
                    .transpose()?
            },
            visible: get(row, "visible")?,
        })
    }
}
