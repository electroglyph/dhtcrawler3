//! Data types passed to and returned from [`crate::Store`].

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, Utc};
use dc3_core::{AnyKey, DhtKey, InfoHashV2};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;

use crate::{Result, StoreError, get, to_u64};

/// Error from parsing one of the enums in this module.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind}: {value:?}")]
pub struct ParseEnumError {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident, $kind:literal, { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// Every variant.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// The text stored in the database.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ParseEnumError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok($name::$variant),)+
                    _ => Err(ParseEnumError { kind: $kind, value: s.to_owned() }),
                }
            }
        }
    };
}

text_enum!(
    /// Why a key is on the denylist.
    DenyReason, "deny reason", {
        Dmca => "dmca",
        Csam => "csam",
        /// Set by the crawler when a name or path matches the blocked terms.
        CsamAuto => "csam-auto",
        Abuse => "abuse",
        /// Set by the crawler for BEP 27 private torrents.
        Private => "private",
        Other => "other",
    }
);

text_enum!(
    /// The reason a visitor gave for a report.
    ReportReason, "report reason", {
        Csam => "csam",
        Copyright => "copyright",
        Malware => "malware",
        Other => "other",
    }
);

text_enum!(
    /// Review state of a report.
    ReportStatus, "report status", {
        Open => "open",
        Actioned => "actioned",
        Dismissed => "dismissed",
    }
);

fn parse_enum<T: FromStr<Err = ParseEnumError>>(s: &str) -> Result<T> {
    s.parse()
        .map_err(|e: ParseEnumError| StoreError::Corrupt(e.to_string()))
}

/// What to do when resolving a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportAction {
    /// Deny the reported key (see [`crate::Store::deny`]) and mark the report actioned.
    Deny(DenyReason),
    /// Mark the report dismissed and un-hide the torrent unless another open
    /// CSAM report exists for it.
    Dismiss,
}

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
    pub change_seq: i64,
    /// Hidden pending review (a CSAM report).
    pub hidden_at: Option<DateTime<Utc>>,
    /// When an admin last dismissed a report about the torrent; it is then
    /// not hidden again automatically. Moderation history, so it is left out
    /// of the serialized form.
    #[serde(skip_serializing)]
    pub reviewed_at: Option<DateTime<Utc>>,
    /// Tombstoned by a denial.
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Every column of [`TorrentRecord::from_row`] except `files`, for table
/// alias `t`.
macro_rules! torrent_columns_base {
    () => {
        "t.id, t.dht_key, t.info_hash_v1, t.info_hash_v2, t.name, t.total_size, t.file_count, \
         t.files_truncated, t.piece_length, t.seen_count, t.first_seen_at, \
         t.last_seen_at, t.change_seq, t.hidden_at, t.reviewed_at, t.deleted_at"
    };
}
pub(crate) use torrent_columns_base;

/// SQL expression: the row (alias `t`) may be shown and indexed.
macro_rules! visible_sql {
    () => {
        "(t.hidden_at IS NULL AND t.deleted_at IS NULL \
          AND NOT dc3_key_denied(t.dht_key, t.info_hash_v1, t.info_hash_v2))"
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

pub(crate) fn any_key_col(row: &PgRow, col: &str) -> Result<AnyKey> {
    let b: Vec<u8> = get(row, col)?;
    any_key_from_bytes(&b).map_err(|_| StoreError::Corrupt(format!("{col}: {} bytes", b.len())))
}

/// Interprets 20 bytes as a DHT/v1 key and 32 bytes as a v2 infohash.
pub(crate) fn any_key_from_bytes(b: &[u8]) -> Result<AnyKey, ()> {
    if let Ok(k) = DhtKey::from_slice(b) {
        return Ok(AnyKey::V1OrDht(k));
    }
    InfoHashV2::from_slice(b).map(AnyKey::V2).map_err(|_| ())
}

/// Raw bytes of an [`AnyKey`].
pub(crate) fn any_key_bytes(k: &AnyKey) -> &[u8] {
    match k {
        AnyKey::V1OrDht(k) => k.as_bytes(),
        AnyKey::V2(h) => h.as_bytes(),
    }
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
            change_seq: get(row, "change_seq")?,
            hidden_at: get(row, "hidden_at")?,
            reviewed_at: get(row, "reviewed_at")?,
            deleted_at: get(row, "deleted_at")?,
        })
    }

    /// True when the row is neither hidden nor tombstoned. (Rows returned by
    /// [`crate::Store::get_by_key`] and [`crate::Store::get_many`] are also
    /// checked against the denylist.)
    pub fn is_live(&self) -> bool {
        self.hidden_at.is_none() && self.deleted_at.is_none()
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
            scrape_failures: u32::try_from(failures)
                .map_err(|_| StoreError::Corrupt(format!("negative scrape_failures: {failures}")))?,
            last_seen_at: get(row, "last_seen_at")?,
            change_seq: get(row, "change_seq")?,
        })
    }
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
    /// Keys skipped because they are denylisted.
    pub denied: u64,
    /// New keys not queued because the queue was full and they were not
    /// priority keys.
    pub dropped: u64,
}

/// Result of [`crate::Store::deny`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenyOutcome {
    /// False when the key was already on the denylist.
    pub newly_denied: bool,
    /// Number of torrent rows tombstoned by this call.
    pub tombstoned: u64,
}

/// One denylist row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenyEntry {
    pub key: AnyKey,
    pub reason: DenyReason,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub created_by: String,
}

impl DenyEntry {
    pub(crate) fn from_row(row: &PgRow) -> Result<DenyEntry> {
        let reason: String = get(row, "reason")?;
        Ok(DenyEntry {
            key: any_key_col(row, "key")?,
            reason: parse_enum(&reason)?,
            note: get(row, "note")?,
            created_at: get(row, "created_at")?,
            created_by: get(row, "created_by")?,
        })
    }
}

/// A report as submitted by a visitor. No IP address is ever stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReport {
    /// The key the visitor reported. The database links the report to the
    /// torrent this key names, if one is stored.
    pub key: AnyKey,
    pub reason: ReportReason,
    /// At most [`crate::REPORT_MESSAGE_MAX_CHARS`] characters.
    pub message: String,
    /// At most [`crate::REPORT_CONTACT_MAX_CHARS`] characters.
    pub contact: Option<String>,
}

/// Result of [`crate::Store::submit_report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubmitOutcome {
    pub report_id: i64,
    /// The report hid the torrent (a CSAM auto-hide).
    pub hidden: bool,
    /// The report would have hidden the torrent, but the hourly auto-hide
    /// budget was used up.
    pub budget_exhausted: bool,
}

/// A stored report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub id: i64,
    pub torrent_id: Option<i64>,
    pub key: AnyKey,
    pub reason: ReportReason,
    pub message: String,
    pub contact: Option<String>,
    pub created_at: DateTime<Utc>,
    pub status: ReportStatus,
    pub resolved_at: Option<DateTime<Utc>>,
    pub resolved_by: Option<String>,
    pub resolution_note: Option<String>,
}

impl Report {
    pub(crate) fn from_row(row: &PgRow) -> Result<Report> {
        let reason: String = get(row, "reason")?;
        let status: String = get(row, "status")?;
        Ok(Report {
            id: get(row, "id")?,
            torrent_id: get(row, "torrent_id")?,
            key: any_key_col(row, "dht_key")?,
            reason: parse_enum(&reason)?,
            message: get(row, "message")?,
            contact: get(row, "contact")?,
            created_at: get(row, "created_at")?,
            status: parse_enum(&status)?,
            resolved_at: get(row, "resolved_at")?,
            resolved_by: get(row, "resolved_by")?,
            resolution_note: get(row, "resolution_note")?,
        })
    }
}

/// Pipeline counters for one UTC day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DailyStats {
    pub day: NaiveDate,
    pub discovered: u64,
    pub fetched: u64,
    pub fetch_failed: u64,
    pub blocked: u64,
}

impl DailyStats {
    pub(crate) fn from_row(row: &PgRow) -> Result<DailyStats> {
        Ok(DailyStats {
            day: get(row, "day")?,
            discovered: to_u64("discovered", get(row, "discovered")?)?,
            fetched: to_u64("fetched", get(row, "fetched")?)?,
            fetch_failed: to_u64("fetch_failed", get(row, "fetch_failed")?)?,
            blocked: to_u64("blocked", get(row, "blocked")?)?,
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
    pub denylisted: u64,
    pub open_reports: u64,
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

/// A stored torrent for the policy rescan ([`crate::Store::scan_live`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRow {
    pub id: i64,
    pub dht_key: DhtKey,
    pub name: String,
    /// Every stored file path, in stored order.
    pub paths: Vec<String>,
}

impl LiveRow {
    pub(crate) fn from_row(row: &PgRow) -> Result<LiveRow> {
        Ok(LiveRow {
            id: get(row, "id")?,
            dht_key: dht_key_col(row, "dht_key")?,
            name: get(row, "name")?,
            paths: get(row, "paths")?,
        })
    }
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
    /// False when the row is hidden, tombstoned or any of its keys is
    /// denylisted; the indexer must then delete the document.
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
            visible: get(row, "visible")?,
        })
    }
}
