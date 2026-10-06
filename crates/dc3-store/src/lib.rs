//! PostgreSQL storage for dhtcrawler3 (`docs/03-design.md` §10; R15, R18).
//!
//! The migrations in `migrations/` are the specification of the schema. This
//! crate wraps a [`sqlx::PgPool`] in [`Store`] and exposes one method per
//! operation. Each role connects as its own database user (R12), and each
//! method names the user it needs:
//!
//! * **crawl** (`dc3_crawler`): [`Store::observe`], [`Store::claim`],
//!   [`Store::renew`], [`Store::complete`], [`Store::fail`],
//!   [`Store::purge_gave_up`], [`Store::pending_depth`],
//!   [`Store::claim_scrape_due`], [`Store::record_scrape`],
//!   [`Store::tombstone_dead`], [`Store::purge_tombstoned`],
//!   [`Store::note_removal`], [`Store::note_removed_sighting`],
//!   [`Store::note_removed_sightings`], [`Store::removal_cooldown_remaining`],
//!   [`Store::removal_cooldowns`], [`Store::refresh_scraped`],
//!   [`Store::removed_keys_count`],
//!   [`Store::is_denied`], [`Store::deny`], [`Store::scan_live`];
//! * **index** (`dc3_indexer`): [`Store::high_water_mark`],
//!   [`Store::changes_since`];
//! * **web** (`dc3_web`): [`Store::get_by_key`], [`Store::get_many`],
//!   [`Store::submit_report`], [`Store::public_stats`],
//!   [`Store::daily_stats`];
//! * **admin** (`dc3_owner`): [`Store::migrate`], [`Store::stats`],
//!   [`Store::undeny`], [`Store::list_denied`], [`Store::list_reports`],
//!   [`Store::get_report`], [`Store::resolve_report`], [`Store::audit`],
//!   [`Store::get_setting`], [`Store::set_setting`].
//!
//! [`Store::ping`] works for every user, and the owner may call every method.
//! A method called by a user without the grant fails with
//! [`StoreError::Database`] (SQLSTATE 42501).
//!
//! # Change feed
//!
//! Every write the search index must see sets `torrents.change_seq =
//! nextval('change_seq')`, and first takes
//! `pg_advisory_xact_lock_shared(CHANGE_LOCK_KEY)` in the same transaction,
//! before any other lock. [`Store::high_water_mark`] takes the same lock
//! exclusively, which waits for every writer that may hold an uncommitted
//! sequence number, so every number at or below the mark belongs to a
//! finished transaction and none is skipped. It waits at most
//! [`HWM_LOCK_TIMEOUT`]; every connection also ends abandoned transactions
//! after [`IDLE_IN_TRANSACTION_TIMEOUT`] (see [`session_options`]).
//!
//! # Denial
//!
//! A denylist key is 20 bytes (DHT key or v1 infohash) or 32 bytes (v2
//! infohash). A v2 torrent is found in the DHT under the first 20 bytes of its
//! hash, so keys are matched by their 20-byte prefix everywhere.
//!
//! # Reports
//!
//! The web user writes nothing directly. [`Store::submit_report`] calls the
//! `submit_report` database function (`SECURITY DEFINER`), which stores the
//! report and may hide the torrent, within the limits in the `settings` table.
//!
//! All SQL uses bound parameters; strings are never concatenated into SQL.
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]

mod admin;
mod crawler;
mod indexer;
mod types;
mod web;

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::time::Duration;

use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgConnection, PgPool, Row};

/// The `sqlx` version this crate uses, for callers that need its types (for
/// example the error inside [`StoreError::Database`]).
pub use sqlx;
/// Connection options for [`Store::connect_with`].
pub use sqlx::postgres::PgConnectOptions;
pub use types::{
    DailyStats, DenyEntry, DenyOutcome, DenyReason, FileRow, IndexRow, LiveRow, NewReport,
    NewTorrent, Observation, ObserveOutcome, ParseEnumError, PendingItem, PublicStats, Report,
    ReportAction, ReportReason, ReportStatus, ScrapeItem, StoreStats, SubmitOutcome, TorrentRecord,
};

/// Advisory lock key of the change feed (ASCII `"dc3chg"` followed by 0x0001).
/// The `submit_report` database function uses the same value.
pub const CHANGE_LOCK_KEY: i64 = 0x6463_3363_6867_0001;

/// Advisory lock key that serialises the CSAM auto-hide budget check inside
/// the `submit_report` database function (ASCII `"dc3hid"` followed by 0x0001).
pub const AUTOHIDE_LOCK_KEY: i64 = 0x6463_3368_6964_0001;

/// First argument of the two-key advisory locks that serialise writes per
/// torrent key (a separate lock space from the one-key locks above).
pub const KEY_LOCK_CLASS: i32 = 0x6463_336b; // "dc3k"

/// Maximum characters in a stored torrent name.
pub const MAX_NAME_CHARS: usize = 1024;
/// Maximum characters in a stored file path.
pub const MAX_PATH_CHARS: usize = 4096;
/// Maximum number of file rows stored per torrent.
pub const MAX_STORED_FILES: usize = 2000;
/// Maximum characters in a report message.
pub const REPORT_MESSAGE_MAX_CHARS: usize = 2000;
/// Maximum characters in a report contact field.
pub const REPORT_CONTACT_MAX_CHARS: usize = 320;
/// Maximum characters in a denial or resolution note.
pub const NOTE_MAX_CHARS: usize = 2000;
/// Maximum characters in an actor name (audit log, denylist, reports).
pub const ACTOR_MAX_CHARS: usize = 128;
/// Maximum characters in an audit action.
pub const AUDIT_ACTION_MAX_CHARS: usize = 64;
/// Maximum characters in an audit subject.
pub const AUDIT_SUBJECT_MAX_CHARS: usize = 256;

/// Largest number of rows one [`Store::claim`] call may lease.
pub const MAX_CLAIM: i64 = 10_000;
/// Largest `limit` accepted by the listing queries.
pub const MAX_PAGE: i64 = 10_000;
/// Largest `limit` accepted by [`Store::changes_since`] and
/// [`Store::scan_live`].
pub const MAX_FEED_PAGE: i64 = 1_000;
/// [`Store::changes_since`] and [`Store::scan_live`] stop adding rows once
/// the rows so far hold this many bytes of name and file-list text. A page
/// always holds at least one row, so it may exceed the budget by one row.
pub const FEED_PAGE_MAX_BYTES: i64 = 64 * 1024 * 1024;
/// Largest number of ids one [`Store::get_many`] call may request.
pub const MAX_GET_MANY: usize = 1_000;
/// [`Store::get_many`] returns at most this many file rows per torrent.
pub const GET_MANY_MAX_FILES: usize = 10;
/// [`Store::get_by_key`] returns the first file rows whose paths together hold
/// at most this many bytes, and sets `files_truncated` when it leaves any out.
pub const GET_BY_KEY_MAX_PATH_BYTES: i64 = 256 * 1024;
/// Largest number of days [`Store::daily_stats`] returns.
pub const MAX_DAILY_STATS_DAYS: u32 = 366;
/// Keys per statement inside one [`Store::observe`] transaction.
pub const OBSERVE_CHUNK: usize = 5_000;

/// Base retry delay after a failed fetch; doubled per earlier attempt.
pub const FAIL_BASE_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// Upper bound of the retry delay.
pub const FAIL_MAX_BACKOFF: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// A key gives up after this many failed attempts.
pub const MAX_FETCH_ATTEMPTS: i32 = 6;

/// Row counts are exact up to this many rows. Above it, [`Store::stats`],
/// [`Store::public_stats`] and [`Store::pending_depth`] report the planner's
/// estimate (`pg_class.reltuples`) instead of scanning the table.
pub const EXACT_COUNT_THRESHOLD: i64 = 1_000_000;

/// Most rows [`Store::trim_removed_keys`] keeps: removal memory is an LRU
/// over `removed_at`, so a flap storm cannot grow `removed_keys` forever.
pub const MAX_REMOVED_KEYS: i64 = 1_000_000;

/// How long [`Store::high_water_mark`] waits for the change-feed lock.
pub const HWM_LOCK_TIMEOUT: Duration = Duration::from_millis(200);
/// `idle_in_transaction_session_timeout` of every store connection.
pub const IDLE_IN_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(60);

/// Setting: CSAM auto-hides allowed per hour (`web.autohide_per_hour`).
pub const SETTING_AUTOHIDE_PER_HOUR: &str = "autohide_per_hour";
/// Setting: `submit_report` refuses new reports while this many are open.
pub const SETTING_OPEN_REPORTS_CAP: &str = "open_reports_cap";
/// Value of [`SETTING_AUTOHIDE_PER_HOUR`] after the first migration, and
/// the value the database function uses if the row is missing.
pub const DEFAULT_AUTOHIDE_PER_HOUR: u32 = 60;
/// Value of [`SETTING_OPEN_REPORTS_CAP`] after the first migration, and the
/// value the database function uses if the row is missing.
pub const DEFAULT_OPEN_REPORTS_CAP: u32 = 100_000;
/// Maximum characters in a setting key.
pub const MAX_SETTING_KEY_CHARS: usize = 64;
/// Maximum characters in a setting value.
pub const MAX_SETTING_VALUE_CHARS: usize = 1024;
/// Largest value of a numeric setting (at most nine decimal digits).
pub const MAX_NUMERIC_SETTING: u32 = 999_999_999;

/// SQLSTATE the `submit_report` database function raises when the
/// open-report cap is reached; mapped to [`StoreError::ReportsFull`].
pub const SQLSTATE_REPORTS_FULL: &str = "D3R01";

/// Settings whose value must be a decimal integer in
/// `0..=MAX_NUMERIC_SETTING` (the schema checks the same).
const NUMERIC_SETTINGS: [&str; 2] = [SETTING_AUTOHIDE_PER_HOUR, SETTING_OPEN_REPORTS_CAP];
/// Digits in [`MAX_NUMERIC_SETTING`].
const MAX_NUMERIC_SETTING_DIGITS: usize = 9;
/// Actor recorded in the audit log by [`Store::set_setting`].
const SETTINGS_ACTOR: &str = "owner";

/// lock_not_available: a lock wait exceeded `lock_timeout`.
const SQLSTATE_LOCK_NOT_AVAILABLE: &str = "55P03";
/// invalid_parameter_value: raised by `submit_report` for bad arguments.
const SQLSTATE_INVALID_PARAMETER: &str = "22023";
/// check_violation.
const SQLSTATE_CHECK_VIOLATION: &str = "23514";

/// Durations passed to SQL are clamped to this (about 100 years).
const MAX_INTERVAL_SECS: f64 = 100.0 * 365.25 * 24.0 * 3600.0;
/// How long [`Store::connect`] waits for a pooled connection.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
/// Length of a DHT key and of a denylist key prefix.
const PREFIX_LEN: usize = 20;
/// Length of a v2 infohash.
const V2_LEN: usize = 32;

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("invalid input: {0}")]
    Invalid(String),
    /// One of the torrent's keys is on the denylist.
    #[error("key is denylisted")]
    Denied,
    #[error("not found")]
    NotFound,
    #[error("report {0} is not open")]
    ReportNotOpen(i64),
    /// [`Store::submit_report`] refused the report: the number of open
    /// reports has reached the `open_reports_cap` setting.
    #[error("too many open reports")]
    ReportsFull,
    /// A value read from the database does not fit the Rust type.
    #[error("unexpected data in database: {0}")]
    Corrupt(String),
}

/// Result alias for this crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// The embedded migrations.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Handle to the database; cheap to clone.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects a pool of at most `max_connections` connections to `url`
    /// (`postgres://user:password@host:port/database`), with the session
    /// settings of [`session_options`].
    pub async fn connect(url: &str, max_connections: u32) -> Result<Store> {
        let options: PgConnectOptions = url.parse()?;
        Store::connect_with(options, max_connections).await
    }

    /// Like [`Store::connect`], from options built by the caller, for example
    /// with a password read from a file, which then needs no URL escaping:
    /// `PgConnectOptions::new_without_pgpass().host(h).port(p).database(d)
    /// .username(u).password(pw)`, or a parsed URL with `.username()` and
    /// `.password()` applied. The `Debug` output of [`PgConnectOptions`]
    /// includes the password, so never log it.
    pub async fn connect_with(options: PgConnectOptions, max_connections: u32) -> Result<Store> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(session_options(options))
            .await?;
        Ok(Store { pool })
    }

    /// Wraps an existing pool. Build its connect options with
    /// [`session_options`] so that its connections get the store's session
    /// settings.
    pub fn from_pool(pool: PgPool) -> Store {
        Store { pool }
    }

    /// The underlying pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies pending migrations. Owner only.
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Checks that the database answers. Any user.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

/// Adds the session settings every store connection needs. A transaction left
/// idle for [`IDLE_IN_TRANSACTION_TIMEOUT`] is ended by the server, so a stuck
/// writer cannot hold the change-feed lock and stall the indexer.
pub fn session_options(options: PgConnectOptions) -> PgConnectOptions {
    options.options([(
        "idle_in_transaction_session_timeout",
        format!("{}s", IDLE_IN_TRANSACTION_TIMEOUT.as_secs()),
    )])
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Takes the change-feed lock in shared mode for the current transaction.
/// Call it before taking any other lock.
async fn lock_change_shared(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(CHANGE_LOCK_KEY)
        .execute(conn)
        .await?;
    Ok(())
}

/// Takes the per-key write locks for the 20-byte prefixes of `keys`, in a
/// fixed order so that concurrent writers cannot deadlock. Call after
/// [`lock_change_shared`].
async fn lock_keys(conn: &mut PgConnection, keys: &[&[u8]]) -> Result<()> {
    let mut ids: Vec<i32> = keys.iter().map(|k| key_lock_id(k)).collect();
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(KEY_LOCK_CLASS)
            .bind(id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Lock id of a key: its first four bytes (keys are uniformly distributed
/// hashes, so collisions only serialise unrelated writes).
fn key_lock_id(key: &[u8]) -> i32 {
    let mut b = [0u8; 4];
    for (dst, src) in b.iter_mut().zip(key.iter()) {
        *dst = *src;
    }
    i32::from_be_bytes(b)
}

/// The 20-byte prefix under which a key is matched.
fn prefix(key: &[u8]) -> &[u8] {
    key.get(..PREFIX_LEN).unwrap_or(key)
}

/// Adds to today's (UTC) counter `column`. `column` is one of a fixed set.
async fn bump_daily(conn: &mut PgConnection, column: DailyCounter, n: i64) -> Result<()> {
    if n <= 0 {
        return Ok(());
    }
    let sql = match column {
        DailyCounter::Discovered => {
            "INSERT INTO stats_daily (day, discovered) VALUES ((now() AT TIME ZONE 'UTC')::date, $1) \
             ON CONFLICT (day) DO UPDATE SET discovered = stats_daily.discovered + EXCLUDED.discovered"
        }
        DailyCounter::Fetched => {
            "INSERT INTO stats_daily (day, fetched) VALUES ((now() AT TIME ZONE 'UTC')::date, $1) \
             ON CONFLICT (day) DO UPDATE SET fetched = stats_daily.fetched + EXCLUDED.fetched"
        }
        DailyCounter::FetchFailed => {
            "INSERT INTO stats_daily (day, fetch_failed) VALUES ((now() AT TIME ZONE 'UTC')::date, $1) \
             ON CONFLICT (day) DO UPDATE SET fetch_failed = stats_daily.fetch_failed + EXCLUDED.fetch_failed"
        }
        DailyCounter::Blocked => {
            "INSERT INTO stats_daily (day, blocked) VALUES ((now() AT TIME ZONE 'UTC')::date, $1) \
             ON CONFLICT (day) DO UPDATE SET blocked = stats_daily.blocked + EXCLUDED.blocked"
        }
    };
    sqlx::query(sql).bind(n).execute(conn).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum DailyCounter {
    Discovered,
    Fetched,
    FetchFailed,
    Blocked,
}

/// Tables whose size may be estimated; a fixed set, so no name is ever
/// interpolated into SQL.
#[derive(Debug, Clone, Copy)]
enum CountedTable {
    Torrents,
    Pending,
}

/// The planner's row estimate for `table`: `pg_class.reltuples`, which is -1
/// before the first VACUUM or ANALYZE. Readable by every user.
async fn estimated_rows(pool: &PgPool, table: CountedTable) -> Result<i64> {
    let sql = match table {
        CountedTable::Torrents => "SELECT reltuples FROM pg_class WHERE oid = 'torrents'::regclass",
        CountedTable::Pending => "SELECT reltuples FROM pg_class WHERE oid = 'pending'::regclass",
    };
    let estimate: f32 = sqlx::query_scalar(sql).fetch_one(pool).await?;
    // `as` saturates at the i64 bounds; the estimate is a whole number.
    #[allow(clippy::cast_possible_truncation)]
    Ok(estimate as i64)
}

/// Torrents that are neither hidden nor tombstoned: exact while the estimate
/// is at most [`EXACT_COUNT_THRESHOLD`], otherwise the estimate itself (which
/// includes hidden and tombstoned rows), because an exact count scans the
/// table.
async fn live_torrents(pool: &PgPool) -> Result<i64> {
    let estimate = estimated_rows(pool, CountedTable::Torrents).await?;
    if estimate > EXACT_COUNT_THRESHOLD {
        return Ok(estimate);
    }
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM torrents WHERE hidden_at IS NULL AND deleted_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// Inserts one audit row.
async fn write_audit(
    conn: &mut PgConnection,
    actor: &str,
    action: &str,
    subject: &str,
    detail: &serde_json::Value,
) -> Result<()> {
    check_actor(actor)?;
    check_len("audit action", action, 1, AUDIT_ACTION_MAX_CHARS)?;
    check_len("audit subject", subject, 0, AUDIT_SUBJECT_MAX_CHARS)?;
    sqlx::query("INSERT INTO audit_log (actor, action, subject, detail) VALUES ($1, $2, $3, $4)")
        .bind(actor)
        .bind(action)
        .bind(subject)
        .bind(detail)
        .execute(conn)
        .await?;
    Ok(())
}

fn check_actor(actor: &str) -> Result<()> {
    check_len("actor", actor, 1, ACTOR_MAX_CHARS)
}

fn check_len(what: &str, s: &str, min: usize, max: usize) -> Result<()> {
    let n = s.chars().count();
    if n < min || n > max {
        return Err(StoreError::Invalid(format!(
            "{what} must be {min}..={max} characters, got {n}"
        )));
    }
    Ok(())
}

fn check_note(note: Option<&str>) -> Result<()> {
    match note {
        Some(n) => check_len("note", n, 0, NOTE_MAX_CHARS),
        None => Ok(()),
    }
}

/// Validates a denylist or report key (20 or 32 bytes).
fn check_key_len(key: &[u8]) -> Result<()> {
    match key.len() {
        PREFIX_LEN | V2_LEN => Ok(()),
        n => Err(StoreError::Invalid(format!(
            "key must be {PREFIX_LEN} or {V2_LEN} bytes, got {n}"
        ))),
    }
}

/// True for the values a numeric setting accepts: 1 to 9 ASCII digits.
fn is_numeric_setting_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NUMERIC_SETTING_DIGITS
        && value.bytes().all(|b| b.is_ascii_digit())
}

/// The SQLSTATE of a database error.
fn sqlstate(e: &sqlx::Error) -> Option<Cow<'_, str>> {
    e.as_database_error().and_then(|d| d.code())
}

/// True when `e` is a database error with SQLSTATE `code`.
fn has_sqlstate(e: &sqlx::Error, code: &str) -> bool {
    sqlstate(e).is_some_and(|c| c == code)
}

/// The server's message of a database error (our own text for the errors
/// raised by our functions and constraints).
fn db_message(e: &sqlx::Error) -> String {
    e.as_database_error()
        .map(|d| d.message().to_owned())
        .unwrap_or_default()
}

/// Lowercase hex of arbitrary bytes.
fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        // Writing to a String cannot fail.
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Clamps a duration to SQL-friendly seconds.
fn secs(d: Duration) -> f64 {
    d.as_secs_f64().min(MAX_INTERVAL_SECS)
}

fn to_i64(what: &str, v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| StoreError::Invalid(format!("{what} {v} exceeds i64")))
}

fn to_u64(what: &str, v: i64) -> Result<u64> {
    u64::try_from(v).map_err(|_| StoreError::Corrupt(format!("negative {what}: {v}")))
}

/// A `usize` count as `u64` (lossless on every supported target).
fn count_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn get<'r, T>(row: &'r PgRow, col: &str) -> Result<T>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    Ok(row.try_get(col)?)
}
