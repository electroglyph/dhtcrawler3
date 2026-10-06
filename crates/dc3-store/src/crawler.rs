//! Crawler operations: observation, the lease queue, completion, denial and
//! the policy rescan. Every method here needs the `dc3_crawler` user (or the
//! owner).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use dc3_core::DhtKey;
use sqlx::PgConnection;

use crate::types::{
    DenyOutcome, DenyReason, LiveRow, NewTorrent, Observation, ObserveOutcome, PendingItem,
    ScrapeItem, files_to_json,
};
use crate::{
    CountedTable, DailyCounter, EXACT_COUNT_THRESHOLD, FAIL_BASE_BACKOFF, FAIL_MAX_BACKOFF,
    FEED_PAGE_MAX_BYTES, MAX_CLAIM, MAX_FEED_PAGE, MAX_FETCH_ATTEMPTS, MAX_NAME_CHARS,
    MAX_PATH_CHARS, MAX_STORED_FILES, OBSERVE_CHUNK, Result, Store, StoreError, bump_daily,
    check_actor, check_key_len, check_len, check_note, count_u64, estimated_rows, get, hex_of,
    lock_change_shared, lock_keys, prefix, secs, to_i64, write_audit,
};

/// Bumps `seen_count` of torrents stored under the given DHT keys. `change_seq`
/// moves only when floor(log2(seen_count)) changes: for positive a and b the
/// highest set bit is equal exactly when (a XOR b) < (a AND b).
const OBSERVE_KNOWN_SQL: &str = "\
WITH input(k, n) AS (SELECT * FROM unnest($1::bytea[], $2::bigint[]))
UPDATE torrents t
   SET seen_count = t.seen_count + i.n,
       last_seen_at = now(),
       change_seq = CASE WHEN (t.seen_count # (t.seen_count + i.n)) > (t.seen_count & (t.seen_count + i.n))
                         THEN nextval('change_seq') ELSE t.change_seq END
  FROM input i
 WHERE t.dht_key = i.k
RETURNING i.k";

/// Same, for keys that are another name of a stored torrent (its v1 infohash or
/// truncated v2 infohash, while the row is stored under a different DHT key).
const OBSERVE_ALIAS_SQL: &str = "\
WITH input(k, n) AS (SELECT * FROM unnest($1::bytea[], $2::bigint[]))
UPDATE torrents t
   SET seen_count = t.seen_count + i.n,
       last_seen_at = now(),
       change_seq = CASE WHEN (t.seen_count # (t.seen_count + i.n)) > (t.seen_count & (t.seen_count + i.n))
                         THEN nextval('change_seq') ELSE t.change_seq END
  FROM input i
 WHERE t.info_hash_v1 = i.k OR substring(t.info_hash_v2 FROM 1 FOR 20) = i.k
RETURNING i.k";

const OBSERVE_DENIED_SQL: &str = "\
SELECT DISTINCT substring(d.key FROM 1 FOR 20) AS k
  FROM denylist d
 WHERE substring(d.key FROM 1 FOR 20) = ANY($1::bytea[])";

/// Queues new keys and counts sightings of keys already queued.
///
/// No `seeders_est` backfill here: a queued key has no `torrents` row by
/// construction (a matching row counts as known and is bumped instead), so
/// there is nothing to join. Retries learn liveness through
/// [`Store::note_fetch_estimate`] instead (LB-21): first attempts are
/// always NULL, ordering helps retries.
const OBSERVE_QUEUE_SQL: &str = "\
INSERT INTO pending AS p (dht_key, seen_count)
SELECT k, n FROM unnest($1::bytea[], $2::bigint[]) AS i(k, n)
ON CONFLICT (dht_key) DO UPDATE SET seen_count = p.seen_count + EXCLUDED.seen_count
RETURNING (xmax = 0) AS inserted";

/// Counts sightings of keys already queued; queues nothing (the queue is full).
const OBSERVE_COUNT_ONLY_SQL: &str = "\
UPDATE pending p
   SET seen_count = p.seen_count + i.n
  FROM unnest($1::bytea[], $2::bigint[]) AS i(k, n)
 WHERE p.dht_key = i.k
RETURNING p.dht_key";

const CLAIM_SQL: &str = "\
UPDATE pending p
   SET lease_until = now() + make_interval(secs => $2)
 WHERE p.dht_key IN (
       SELECT q.dht_key FROM pending q
        WHERE NOT q.gave_up
          AND q.next_attempt_at <= now()
          AND (q.lease_until IS NULL OR q.lease_until < now())
        ORDER BY q.seeders_est DESC NULLS LAST, q.next_attempt_at
        LIMIT $1
          FOR UPDATE SKIP LOCKED)
RETURNING p.dht_key, p.attempts, p.seen_count";

/// Extends a lease that has not expired; never shortens it.
const RENEW_SQL: &str = "\
UPDATE pending
   SET lease_until = greatest(lease_until, now() + make_interval(secs => $2))
 WHERE dht_key = $1
   AND NOT gave_up
   AND lease_until > now()";

const FAIL_SQL: &str = "\
UPDATE pending
   SET attempts = attempts + 1,
       last_attempt_at = now(),
       lease_until = NULL,
       next_attempt_at = now() + least(make_interval(secs => $2) * power(2, least(attempts, 30)),
                                       make_interval(secs => $3)),
       gave_up = attempts + 1 >= $4
 WHERE dht_key = $1
RETURNING gave_up";

const GIVE_UP_SQL: &str = "\
UPDATE pending
   SET attempts = attempts + 1,
       last_attempt_at = now(),
       lease_until = NULL,
       gave_up = true
 WHERE dht_key = $1
RETURNING gave_up";

const TORRENT_UPDATE_SQL: &str = "\
UPDATE torrents
   SET info_hash_v1 = CASE WHEN $2 THEN info_hash_v1 ELSE $3 END,
       info_hash_v2 = CASE WHEN $4 THEN info_hash_v2 ELSE $5 END,
       name = $6, total_size = $7, file_count = $8, files = $9,
       files_truncated = $10, piece_length = $11,
       seen_count = seen_count + $12,
       last_seen_at = now(),
       deleted_at = NULL,
       change_seq = nextval('change_seq')
 WHERE id = $1";

const TORRENT_INSERT_SQL: &str = "\
INSERT INTO torrents (dht_key, info_hash_v1, info_hash_v2, name, total_size, file_count, files,
                      files_truncated, piece_length, seen_count, first_seen_at, change_seq)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, coalesce($11, now()), nextval('change_seq'))
RETURNING id";

const TOMBSTONE_SQL: &str = "\
UPDATE torrents
   SET name = '', files = '[]'::jsonb, files_truncated = false,
       deleted_at = coalesce(deleted_at, now()),
       change_seq = nextval('change_seq')
 WHERE dht_key = $1 OR info_hash_v1 = $1 OR substring(info_hash_v2 FROM 1 FOR 20) = $1
RETURNING id";

/// Claims rows due for a BEP 33 scrape and stamps the claim (`$2`/`$3` are
/// the unknown/live intervals in seconds). Never-scraped rows
/// (`last_scraped_at IS NULL`) are always due; unaware rows
/// (`seeders_est IS NULL`, scraped before) use the unknown interval so they
/// are re-polled less often; rows with an estimate use the live interval.
/// The live clause is gated on `seeders_est IS NOT NULL`, else it would also
/// match unaware rows and the unknown interval would never apply.
const CLAIM_SCRAPE_SQL: &str = "\
UPDATE torrents t
   SET last_scraped_at = now()
 WHERE t.id IN (
        SELECT s.id FROM torrents s
         WHERE s.deleted_at IS NULL
           AND s.hidden_at IS NULL
           AND NOT dc3_key_denied(s.dht_key, s.info_hash_v1, s.info_hash_v2)
           AND (s.last_scraped_at IS NULL
                OR (s.seeders_est IS NULL
                    AND s.last_scraped_at IS NOT NULL
                    AND s.last_scraped_at < now() - make_interval(secs => $2))
                OR (s.seeders_est IS NOT NULL
                    AND s.last_scraped_at < now() - make_interval(secs => $3)))
         ORDER BY s.last_scraped_at ASC NULLS FIRST
         LIMIT $1
           FOR UPDATE SKIP LOCKED)
RETURNING t.id, t.dht_key, t.seeders_est, t.scrape_failures, t.last_seen_at, t.change_seq";

/// Stats-only scrape write: no `change_seq` bump (the indexer must not see
/// churn) and no `files` touch (which would fire `torrents_files_shape`).
const RECORD_SCRAPE_SQL: &str = "\
UPDATE torrents
   SET seeders_est = $2, scrape_failures = $3, last_scraped_at = now()
 WHERE id = $1";

/// Conditional scrape tombstone: wipes name/files like [`TOMBSTONE_SQL`]
/// (without a denylist insert) but only when the row is still exactly as
/// claimed, so a concurrent fetch is never clobbered.
const TOMBSTONE_DEAD_SQL: &str = "\
UPDATE torrents
   SET name = '', files = '[]'::jsonb, files_truncated = false,
       deleted_at = coalesce(deleted_at, now()),
       change_seq = nextval('change_seq')
 WHERE id = $1 AND deleted_at IS NULL AND last_seen_at = $2 AND change_seq = $3";

/// Deletes scrape tombstones older than the grace, keeping denylist
/// tombstones (the block record) forever.
const PURGE_TOMBSTONED_SQL: &str = "\
DELETE FROM torrents
 WHERE id IN (
        SELECT t.id FROM torrents t
         WHERE t.deleted_at IS NOT NULL
           AND t.deleted_at < now() - make_interval(secs => $1)
           AND NOT dc3_key_denied(t.dht_key, t.info_hash_v1, t.info_hash_v2)
         ORDER BY t.total_size DESC
         LIMIT $2)";

/// Upserts removal memory: a new row starts at one consecutive removal, an
/// existing one escalates while its sighting counter restarts.
const NOTE_REMOVAL_SQL: &str = "\
INSERT INTO removed_keys (key, removed_at, removals, sightings)
VALUES ($1, now(), 1, 0)
ON CONFLICT (key) DO UPDATE
   SET removed_at = now(),
       removals = removed_keys.removals + 1,
       sightings = 0";

/// Rows of a [`Store::scan_live`] page: at most `$2` rows after id `$1`,
/// stopping once the earlier rows hold `$3` bytes of text (the first row is
/// always included). The sizes come from the JSON text of `files`, which is
/// at least as long as its paths.
const SCAN_LIVE_SQL: &str = "\
WITH page AS (
    SELECT t.id, octet_length(t.name)::bigint + octet_length(t.files::text) AS bytes
      FROM torrents t
     WHERE t.id > $1 AND t.deleted_at IS NULL
     ORDER BY t.id
     LIMIT $2
), sized AS (
    SELECT p.id,
           coalesce(sum(p.bytes) OVER (ORDER BY p.id ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0)
               AS before
      FROM page p
)
SELECT t.id, t.dht_key, t.name, x.paths
  FROM sized s
  JOIN torrents t ON t.id = s.id
 CROSS JOIN LATERAL (
       SELECT coalesce(array_agg(f.e ->> 'p' ORDER BY f.o) FILTER (WHERE f.e ->> 'p' IS NOT NULL),
                       '{}') AS paths
         FROM jsonb_array_elements(t.files) WITH ORDINALITY AS f(e, o)) x
 WHERE s.before < $3
 ORDER BY s.id";

impl Store {
    /// Records one batch of sightings in one transaction.
    ///
    /// * Keys of stored torrents get `seen_count += sightings` and
    ///   `last_seen_at = now()`; `change_seq` moves only when
    ///   floor(log2(seen_count)) changes.
    /// * Other keys that are not denylisted add their sightings to
    ///   `pending.seen_count` when already queued, and are otherwise inserted
    ///   into `pending`.
    /// * While [`Store::pending_depth`] is at least `max_pending`, only
    ///   priority keys are inserted; once it reaches 2 × `max_pending`, none
    ///   are. Keys not inserted are counted in [`ObserveOutcome::dropped`].
    ///   The depth is read once, before the transaction, so the limits are
    ///   approximate.
    /// * `stats_daily.discovered` counts newly queued keys.
    ///
    /// Duplicate keys in the batch are merged (a key is a priority key if any
    /// of its entries is); entries with 0 sightings are ignored.
    pub async fn observe(&self, batch: &[Observation], max_pending: i64) -> Result<ObserveOutcome> {
        // Sorted, so concurrent batches lock rows in the same order.
        let mut merged: BTreeMap<[u8; 20], (i64, bool)> = BTreeMap::new();
        for o in batch {
            if o.sightings == 0 {
                continue;
            }
            let e = merged.entry(o.key.0).or_insert((0, false));
            e.0 = e.0.saturating_add(i64::from(o.sightings));
            e.1 |= o.priority;
        }
        let mut out = ObserveOutcome::default();
        if merged.is_empty() {
            return Ok(out);
        }
        // Read before the transaction starts, so the change lock is never held
        // while the queue is counted.
        let gate = if self
            .pending_depth_reaches(max_pending.saturating_mul(2))
            .await?
        {
            QueueGate::Closed
        } else if merged.values().all(|(_, priority)| *priority) {
            QueueGate::Open
        } else if self.pending_depth_reaches(max_pending).await? {
            QueueGate::PriorityOnly
        } else {
            QueueGate::Open
        };
        let entries: Vec<Merged> = merged
            .into_iter()
            .map(|(key, (n, priority))| Merged {
                key: key.to_vec(),
                n,
                priority,
            })
            .collect();

        let mut tx = self.pool.begin().await?;
        lock_change_shared(&mut tx).await?;
        for chunk in entries.chunks(OBSERVE_CHUNK) {
            let c = observe_chunk(&mut tx, chunk, gate).await?;
            out.known = out.known.saturating_add(c.known);
            out.queued = out.queued.saturating_add(c.queued);
            out.denied = out.denied.saturating_add(c.denied);
            out.dropped = out.dropped.saturating_add(c.dropped);
        }
        bump_daily(
            &mut tx,
            DailyCounter::Discovered,
            i64::try_from(out.queued).unwrap_or(i64::MAX),
        )
        .await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Leases up to `n` due queue items for `lease`. Items whose lease expires
    /// (a crashed worker) become claimable again.
    pub async fn claim(&self, n: i64, lease: Duration) -> Result<Vec<PendingItem>> {
        if n <= 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(CLAIM_SQL)
            .bind(n.min(MAX_CLAIM))
            .bind(secs(lease))
            .fetch_all(&self.pool)
            .await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let attempts: i32 = get(row, "attempts")?;
            let seen: i64 = get(row, "seen_count")?;
            items.push(PendingItem {
                dht_key: crate::types::dht_key_col(row, "dht_key")?,
                attempts: u32::try_from(attempts)
                    .map_err(|_| StoreError::Corrupt(format!("attempts {attempts}")))?,
                seen_count: crate::to_u64("seen_count", seen)?,
            });
        }
        Ok(items)
    }

    /// Extends the lease on a claimed key to at least `lease` from now.
    /// Returns false when the key is not queued, has given up, or its lease
    /// has already expired (another worker may have claimed it since).
    ///
    /// Leases carry no owner, so a worker whose lease expired and was claimed
    /// again extends the new holder's lease. That is harmless: both fetch the
    /// same key, and [`Store::complete`] is idempotent.
    pub async fn renew(&self, key: &DhtKey, lease: Duration) -> Result<bool> {
        let n = sqlx::query(RENEW_SQL)
            .bind(key.as_bytes().as_slice())
            .bind(secs(lease))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// Rows in `pending`, including keys that gave up: exact up to
    /// [`EXACT_COUNT_THRESHOLD`]. When the planner estimates more rows than
    /// that, the estimate (`pg_class.reltuples`) is returned without counting.
    /// When the estimate is stale and low, the count stops at the threshold
    /// plus one.
    pub async fn pending_depth(&self) -> Result<i64> {
        let estimate = estimated_rows(&self.pool, CountedTable::Pending).await?;
        if estimate > EXACT_COUNT_THRESHOLD {
            return Ok(estimate);
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM pending LIMIT $1) p")
            .bind(EXACT_COUNT_THRESHOLD.saturating_add(1))
            .fetch_one(&self.pool)
            .await?;
        Ok(if n > EXACT_COUNT_THRESHOLD {
            n.max(estimate)
        } else {
            n
        })
    }

    /// Whether [`Store::pending_depth`] is at least `max_pending`, without
    /// counting rows that cannot change the answer.
    pub(crate) async fn pending_depth_reaches(&self, max_pending: i64) -> Result<bool> {
        if max_pending <= 0 {
            return Ok(true);
        }
        let estimate = estimated_rows(&self.pool, CountedTable::Pending).await?;
        if estimate > EXACT_COUNT_THRESHOLD {
            return Ok(estimate >= max_pending);
        }
        // pending_depth() would count, and its count stops at the threshold
        // plus one.
        if max_pending > EXACT_COUNT_THRESHOLD.saturating_add(1) {
            return Ok(false);
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM pending LIMIT $1) p")
            .bind(max_pending)
            .fetch_one(&self.pool)
            .await?;
        Ok(n >= max_pending)
    }

    /// Stores a fetched torrent and removes its queue row, in one transaction.
    ///
    /// Upserts by `dht_key`. A row stored under another key with the same v1 or
    /// v2 infohash is the same torrent and is updated instead. On update,
    /// `first_seen_at` is kept and the queue row's `seen_count` is added. A
    /// tombstoned row is restored (possible only after [`Store::undeny`]).
    /// Calling it again is harmless.
    ///
    /// Returns the torrent id, or [`StoreError::Denied`] when any of the
    /// torrent's keys is denylisted; the queue row is removed in that case too.
    /// A torrent that can never be stored (a size above `i64::MAX`, a name or
    /// path over its limit) fails with [`StoreError::Invalid`] before the
    /// database is touched; the caller should then [`Store::give_up`].
    pub async fn complete(&self, key: &DhtKey, t: &NewTorrent) -> Result<i64> {
        validate_torrent(t)?;
        if t.dht_key != *key {
            return Err(StoreError::Invalid(
                "NewTorrent.dht_key differs from the completed key".into(),
            ));
        }
        let files = files_to_json(&t.files)?;
        let total_size = to_i64("total_size", t.total_size)?;
        let file_count = to_i64("file_count", t.file_count)?;
        let piece_length = t
            .piece_length
            .map(|p| to_i64("piece_length", p))
            .transpose()?;
        let v1 = t.info_hash_v1.map(|k| k.0.to_vec());
        let v2 = t.info_hash_v2.map(|h| h.0.to_vec());
        let v2_prefix = t.info_hash_v2.map(|h| h.truncated());

        let mut tx = self.pool.begin().await?;
        lock_change_shared(&mut tx).await?;
        let mut lock_set: Vec<&[u8]> = vec![key.as_bytes()];
        if let Some(k) = &t.info_hash_v1 {
            lock_set.push(k.as_bytes());
        }
        if let Some(k) = &v2_prefix {
            lock_set.push(k.as_bytes());
        }
        lock_keys(&mut tx, &lock_set).await?;

        let pending = sqlx::query(
            "DELETE FROM pending WHERE dht_key = $1 RETURNING seen_count, discovered_at",
        )
        .bind(key.as_bytes().as_slice())
        .fetch_optional(&mut *tx)
        .await?;
        let (add_seen, discovered_at) = match &pending {
            Some(row) => (
                get::<i64>(row, "seen_count")?,
                Some(get::<chrono::DateTime<chrono::Utc>>(row, "discovered_at")?),
            ),
            None => (0, None),
        };

        // A successful fetch is positive liveness proof: clear removal
        // memory, so a resurgent torrent is not penalised forever (§4a).
        sqlx::query("DELETE FROM removed_keys WHERE key = $1")
            .bind(key.as_bytes().as_slice())
            .execute(&mut *tx)
            .await?;

        let denied: bool = sqlx::query_scalar("SELECT dc3_key_denied($1, $2, $3)")
            .bind(key.as_bytes().as_slice())
            .bind(v1.as_deref())
            .bind(v2.as_deref())
            .fetch_one(&mut *tx)
            .await?;
        if denied {
            tx.commit().await?;
            return Err(StoreError::Denied);
        }

        let rows = sqlx::query(
            "SELECT id, dht_key, info_hash_v1, info_hash_v2 FROM torrents \
             WHERE dht_key = $1 OR info_hash_v1 = $2 OR info_hash_v2 = $3 \
             ORDER BY id FOR UPDATE",
        )
        .bind(key.as_bytes().as_slice())
        .bind(v1.as_deref())
        .bind(v2.as_deref())
        .fetch_all(&mut *tx)
        .await?;
        let mut existing = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: i64 = get(row, "id")?;
            let dk: Vec<u8> = get(row, "dht_key")?;
            let r1: Option<Vec<u8>> = get(row, "info_hash_v1")?;
            let r2: Option<Vec<u8>> = get(row, "info_hash_v2")?;
            existing.push((id, dk, r1, r2));
        }
        let target = existing
            .iter()
            .find(|(_, dk, _, _)| dk.as_slice() == key.as_bytes().as_slice())
            .or_else(|| existing.first());

        let (id, counted) = match target {
            Some((id, _, _, _)) => {
                let id = *id;
                // Keep the stored hash if another row already owns the new one.
                let conflict_v1 =
                    v1.is_some() && existing.iter().any(|(o, _, r1, _)| *o != id && *r1 == v1);
                let conflict_v2 =
                    v2.is_some() && existing.iter().any(|(o, _, _, r2)| *o != id && *r2 == v2);
                sqlx::query(TORRENT_UPDATE_SQL)
                    .bind(id)
                    .bind(conflict_v1)
                    .bind(v1.as_deref())
                    .bind(conflict_v2)
                    .bind(v2.as_deref())
                    .bind(t.name.as_str())
                    .bind(total_size)
                    .bind(file_count)
                    .bind(&files)
                    .bind(t.files_truncated)
                    .bind(piece_length)
                    .bind(add_seen)
                    .execute(&mut *tx)
                    .await?;
                (id, pending.is_some())
            }
            None => {
                let id: i64 = sqlx::query_scalar(TORRENT_INSERT_SQL)
                    .bind(key.as_bytes().as_slice())
                    .bind(v1.as_deref())
                    .bind(v2.as_deref())
                    .bind(t.name.as_str())
                    .bind(total_size)
                    .bind(file_count)
                    .bind(&files)
                    .bind(t.files_truncated)
                    .bind(piece_length)
                    .bind(add_seen.max(1))
                    .bind(discovered_at)
                    .fetch_one(&mut *tx)
                    .await?;
                (id, true)
            }
        };
        if counted {
            bump_daily(&mut tx, DailyCounter::Fetched, 1).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Records a failed fetch: `attempts += 1`, retry after
    /// min(5 min × 2^(previous attempts), 7 days), and give up after
    /// [`MAX_FETCH_ATTEMPTS`] attempts. Releases the lease. Returns true when
    /// the key has now given up; false also when no queue row exists.
    pub async fn fail(&self, key: &DhtKey) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let gave_up: Option<bool> = sqlx::query_scalar(FAIL_SQL)
            .bind(key.as_bytes().as_slice())
            .bind(secs(FAIL_BASE_BACKOFF))
            .bind(secs(FAIL_MAX_BACKOFF))
            .bind(MAX_FETCH_ATTEMPTS)
            .fetch_optional(&mut *tx)
            .await?;
        if gave_up.is_some() {
            bump_daily(&mut tx, DailyCounter::FetchFailed, 1).await?;
        }
        tx.commit().await?;
        Ok(gave_up.unwrap_or(false))
    }

    /// Records a fetch that can never succeed (verified metadata that does not
    /// parse or cannot be stored): `attempts += 1` and the key gives up at
    /// once. Releases the lease. Returns false when no queue row exists.
    pub async fn give_up(&self, key: &DhtKey) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let found: Option<bool> = sqlx::query_scalar(GIVE_UP_SQL)
            .bind(key.as_bytes().as_slice())
            .fetch_optional(&mut *tx)
            .await?;
        if found.is_some() {
            bump_daily(&mut tx, DailyCounter::FetchFailed, 1).await?;
        }
        tx.commit().await?;
        Ok(found.is_some())
    }

    /// Deletes queue rows that gave up more than `older_than` ago, so their
    /// keys may be queued again. Returns the number removed.
    pub async fn purge_gave_up(&self, older_than: Duration) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM pending WHERE gave_up AND last_attempt_at < now() - make_interval(secs => $1)",
        )
        .bind(secs(older_than))
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// True when any of `keys` (20 or 32 bytes each) is covered by the
    /// denylist. Keys are compared by their 20-byte prefix, so a v2 infohash
    /// and its truncated DHT key deny each other.
    pub async fn is_denied(&self, keys: &[&[u8]]) -> Result<bool> {
        let prefixes: Vec<Vec<u8>> = keys.iter().map(|k| prefix(k).to_vec()).collect();
        if prefixes.is_empty() {
            return Ok(false);
        }
        let denied: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM denylist d WHERE substring(d.key FROM 1 FOR 20) = ANY($1::bytea[]))",
        )
        .bind(&prefixes)
        .fetch_one(&self.pool)
        .await?;
        Ok(denied)
    }

    /// Denies `key` (20 or 32 bytes) in one transaction: adds it to the
    /// denylist (an existing entry is kept unchanged), tombstones every torrent
    /// whose DHT key, v1 infohash or truncated v2 infohash equals the key's
    /// 20-byte prefix (wiping name and files and bumping `change_seq`), deletes
    /// the queue row, writes the audit log and, for a new entry, adds one to
    /// `stats_daily.blocked`.
    pub async fn deny(
        &self,
        key: &[u8],
        reason: DenyReason,
        note: Option<&str>,
        actor: &str,
    ) -> Result<DenyOutcome> {
        let mut tx = self.pool.begin().await?;
        let out = deny_in_tx(&mut tx, key, reason, note, actor).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Torrents that are not tombstoned (hidden ones included) with
    /// `id > after_id`, in id order, for `policy rescan`. Returns at most
    /// `limit` rows (capped at [`MAX_FEED_PAGE`]) and stops early once the
    /// rows hold [`FEED_PAGE_MAX_BYTES`] of text, so only an empty page means
    /// the scan is done. Continue from the last returned id.
    pub async fn scan_live(&self, after_id: i64, limit: i64) -> Result<Vec<LiveRow>> {
        self.scan_live_within(after_id, limit, FEED_PAGE_MAX_BYTES)
            .await
    }

    /// [`Store::scan_live`] with an explicit byte budget.
    pub(crate) async fn scan_live_within(
        &self,
        after_id: i64,
        limit: i64,
        max_bytes: i64,
    ) -> Result<Vec<LiveRow>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(SCAN_LIVE_SQL)
            .bind(after_id)
            .bind(limit.min(MAX_FEED_PAGE))
            .bind(max_bytes.max(1))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(LiveRow::from_row).collect()
    }

    /// Claims up to `limit` stored torrents due for a BEP 33 scrape, oldest
    /// scrape first (never-scraped first). A row is due when it is visible
    /// (not hidden, tombstoned or denylisted) and either never scraped, or
    /// scraped longer ago than `unknown_interval` (still-unaware rows, whose
    /// `seeders_est` is NULL) or `live_interval` (rows with an estimate).
    ///
    /// The claim stamps `last_scraped_at = now()`, which is also the lease:
    /// rows claimed by a crashed worker become due again once the interval
    /// elapses. The stamp is stats-only otherwise: it never moves
    /// `change_seq`, so the indexer sees no churn.
    pub async fn claim_scrape_due(
        &self,
        limit: i64,
        live_interval: Duration,
        unknown_interval: Duration,
    ) -> Result<Vec<ScrapeItem>> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(CLAIM_SCRAPE_SQL)
            .bind(limit.min(MAX_CLAIM))
            .bind(secs(unknown_interval))
            .bind(secs(live_interval))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(ScrapeItem::from_row).collect()
    }

    /// Records a finished scrape: sets `seeders_est` (or keeps the caller
    /// passing the previous value for unaware scrapes), `scrape_failures`
    /// and `last_scraped_at = now()`. Stats-only: no `change_seq` bump, so
    /// the indexer sees no churn. Returns false when no row has `id`.
    pub async fn record_scrape(
        &self,
        id: i64,
        seeders_est: Option<u32>,
        scrape_failures: u32,
    ) -> Result<bool> {
        let est = seeders_est
            .map(|e| {
                i32::try_from(e)
                    .map_err(|_| StoreError::Invalid(format!("seeders_est {e} exceeds i32")))
            })
            .transpose()?;
        let failures = i32::try_from(scrape_failures).map_err(|_| {
            StoreError::Invalid(format!("scrape_failures {scrape_failures} exceeds i32"))
        })?;
        let n = sqlx::query(RECORD_SCRAPE_SQL)
            .bind(id)
            .bind(est)
            .bind(failures)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// Tombstones a dead torrent and notes the removal in one transaction
    /// (a crash between the two would leave a tombstone without memory, and
    /// the key would be re-fetched on every flap).
    ///
    /// The wipe is conditional on the row being unchanged since the claim
    /// (`last_seen_at` and `change_seq` snapshots): a concurrent fetch that
    /// revived or refreshed the row wins, and this call then records nothing
    /// (returns false, without touching `removed_keys`). Denylist tombstones
    /// are never touched here; only scrape deaths.
    pub async fn tombstone_dead(
        &self,
        id: i64,
        old_last_seen_at: chrono::DateTime<chrono::Utc>,
        old_change_seq: i64,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        lock_change_shared(&mut tx).await?;
        let key: Option<Vec<u8>> = sqlx::query_scalar("SELECT dht_key FROM torrents WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(key) = key else {
            return Ok(false);
        };
        lock_keys(&mut tx, &[key.as_slice()]).await?;
        let n = sqlx::query(TOMBSTONE_DEAD_SQL)
            .bind(id)
            .bind(old_last_seen_at)
            .bind(old_change_seq)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Ok(false);
        }
        sqlx::query(NOTE_REMOVAL_SQL)
            .bind(key.as_slice())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Deletes up to `limit` scrape tombstones older than `grace`, biggest
    /// first, so each sweep frees the most disk (win 5). Denylist tombstones
    /// are never purged: they are the block record. Returns the number
    /// removed. The grace lets the indexer observably drop the document
    /// first (it sees `visible=false` through the change feed).
    pub async fn purge_tombstoned(&self, grace: Duration, limit: i64) -> Result<u64> {
        let res = sqlx::query(PURGE_TOMBSTONED_SQL)
            .bind(secs(grace))
            .bind(limit.max(0))
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Records a seeder estimate observed for free by a fetch scrape lookup
    /// (§8 piggyback): sets `pending.seeders_est` for the next retry's
    /// liveness ordering. Best-effort by design; the caller logs failures.
    /// Unaware lookups must not call this (NULL means unscraped, 0 means
    /// measured dead).
    pub async fn note_fetch_estimate(&self, key: &DhtKey, seeders_est: u32) -> Result<()> {
        let est = i32::try_from(seeders_est).map_err(|_| {
            StoreError::Invalid(format!("seeders_est {seeders_est} exceeds i32"))
        })?;
        sqlx::query("UPDATE pending SET seeders_est = $2 WHERE dht_key = $1")
            .bind(key.as_bytes().as_slice())
            .bind(est)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Notes a removal in `removed_keys` outside [`Store::tombstone_dead`]
    /// (which already notes its own): upserts the row, stamping `removed_at`
    /// and escalating `removals` while resetting `sightings` for the new
    /// cooldown period.
    pub async fn note_removal(&self, key: &[u8]) -> Result<()> {
        check_key_len(key)?;
        sqlx::query(NOTE_REMOVAL_SQL)
            .bind(prefix(key))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Counts a post-removal sighting of a removed key, for the §4a
    /// strong-evidence rule. Only increments an existing row; never creates
    /// one (a sighting alone is not a removal).
    pub async fn note_removed_sighting(&self, key: &[u8]) -> Result<()> {
        check_key_len(key)?;
        sqlx::query("UPDATE removed_keys SET sightings = sightings + 1 WHERE key = $1")
            .bind(prefix(key))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// How long `key` stays out of admission: the cooldown for its
    /// consecutive-removal count minus the time since `removed_at`, or zero
    /// when the key was never removed or the cooldown has expired. Keys are
    /// matched by 20-byte prefix.
    pub async fn removal_cooldown_remaining(&self, key: &[u8]) -> Result<Duration> {
        check_key_len(key)?;
        let row = sqlx::query("SELECT removed_at, removals FROM removed_keys WHERE key = $1")
            .bind(prefix(key))
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(Duration::ZERO);
        };
        let removed_at: chrono::DateTime<chrono::Utc> = get(&row, "removed_at")?;
        let removals: i32 = get(&row, "removals")?;
        Ok(remaining_since(removed_at, removals))
    }

    /// The remaining cooldown of several keys in one round trip: one
    /// `(key, remaining)` pair per `removed_keys` row found (remaining may
    /// be zero for expired rows). Keys are matched by 20-byte prefix.
    /// Admission uses this to skip keys still in cooldown, in batch.
    pub async fn removal_cooldowns(&self, keys: &[DhtKey]) -> Result<Vec<(DhtKey, Duration)>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let raw: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes().as_slice()).collect();
        let rows = sqlx::query(
            "SELECT key, removed_at, removals FROM removed_keys WHERE key = ANY($1::bytea[])",
        )
        .bind(&raw)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let key: Vec<u8> = get(row, "key")?;
            let removed_at: chrono::DateTime<chrono::Utc> = get(row, "removed_at")?;
            let removals: i32 = get(row, "removals")?;
            let key = DhtKey::from_slice(&key)
                .map_err(|e| StoreError::Corrupt(format!("removed_keys.key: {e}")))?;
            out.push((key, remaining_since(removed_at, removals)));
        }
        Ok(out)
    }

    /// Counts post-removal sightings of removed keys, in one statement.
    /// Only increments existing rows; returns the rows touched. Admission
    /// calls this for every batch key it finds still remembered, feeding
    /// the §4a strong-evidence counter.
    pub async fn note_removed_sightings(&self, keys: &[DhtKey]) -> Result<u64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let raw: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes().as_slice()).collect();
        let res = sqlx::query("UPDATE removed_keys SET sightings = sightings + 1 WHERE key = ANY($1::bytea[])")
            .bind(&raw)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// A live seeder just proved itself (§4b win 6, announce short-circuit):
    /// refreshes `last_scraped_at` and resets `scrape_failures` without a
    /// lookup, deferring (never skipping) the next due scrape. Stats-only:
    /// no `change_seq` bump. Tombstoned rows are untouched (only a fetch
    /// revives those); keys without a row simply match nothing. Returns
    /// the rows refreshed.
    pub async fn refresh_scraped(&self, keys: &[DhtKey]) -> Result<u64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let raw: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes().as_slice()).collect();
        let res = sqlx::query(
            "UPDATE torrents SET last_scraped_at = now(), scrape_failures = 0 \
             WHERE dht_key = ANY($1::bytea[]) AND deleted_at IS NULL",
        )
        .bind(&raw)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Rows in `removed_keys`.
    pub async fn removed_keys_count(&self) -> Result<i64> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM removed_keys")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// Deletes the oldest `removed_keys` rows beyond `cap` (LRU over
    /// `removed_at`), so a flap storm cannot grow the table forever.
    /// Returns the number removed. The cap is approximate under concurrency.
    pub async fn trim_removed_keys(&self, cap: i64) -> Result<u64> {
        let n: i64 = self.removed_keys_count().await?.saturating_sub(cap.max(0));
        if n <= 0 {
            return Ok(0);
        }
        let res = sqlx::query(
            "DELETE FROM removed_keys WHERE key IN \
             (SELECT key FROM removed_keys ORDER BY removed_at ASC LIMIT $1)",
        )
        .bind(n)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

/// Cooldown for a key removed `removals` times in a row (§4a): 7d, then 30d,
/// then 90d capped. A successful fetch deletes the row
/// ([`Store::complete`]), so resurgent torrents are not penalised forever.
pub(crate) fn removal_cooldown(removals: i32) -> Duration {
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    if removals <= 1 {
        DAY.saturating_mul(7)
    } else if removals == 2 {
        DAY.saturating_mul(30)
    } else {
        DAY.saturating_mul(90)
    }
}

/// Remaining cooldown of a row removed at `removed_at` with `removals`
/// consecutive removals.
fn remaining_since(removed_at: chrono::DateTime<chrono::Utc>, removals: i32) -> Duration {
    let cooldown = removal_cooldown(removals);
    let elapsed = chrono::Utc::now().signed_duration_since(removed_at);
    if elapsed.num_seconds() < 0 {
        return cooldown;
    }
    match elapsed.to_std() {
        Ok(elapsed) if elapsed < cooldown => cooldown.saturating_sub(elapsed),
        _ => Duration::ZERO,
    }
}

/// Which new keys [`Store::observe`] may insert into `pending`.
#[derive(Clone, Copy)]
enum QueueGate {
    Open,
    PriorityOnly,
    Closed,
}

/// One distinct key of an [`Store::observe`] batch.
struct Merged {
    key: Vec<u8>,
    n: i64,
    priority: bool,
}

async fn observe_chunk(
    conn: &mut PgConnection,
    chunk: &[Merged],
    gate: QueueGate,
) -> Result<ObserveOutcome> {
    let mut out = ObserveOutcome::default();

    let (keys, counts) = columns(chunk.iter());
    let mut known: HashSet<Vec<u8>> = sqlx::query_scalar(OBSERVE_KNOWN_SQL)
        .bind(&keys)
        .bind(&counts)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
    let rest: Vec<&Merged> = chunk.iter().filter(|m| !known.contains(&m.key)).collect();
    if !rest.is_empty() {
        let (keys, counts) = columns(rest.iter().copied());
        let aliases: Vec<Vec<u8>> = sqlx::query_scalar(OBSERVE_ALIAS_SQL)
            .bind(&keys)
            .bind(&counts)
            .fetch_all(&mut *conn)
            .await?;
        known.extend(aliases);
    }
    out.known = count_u64(known.len());

    let rest: Vec<&Merged> = rest
        .into_iter()
        .filter(|m| !known.contains(&m.key))
        .collect();
    if rest.is_empty() {
        return Ok(out);
    }
    let (keys, _) = columns(rest.iter().copied());
    let denied: HashSet<Vec<u8>> = sqlx::query_scalar(OBSERVE_DENIED_SQL)
        .bind(&keys)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect();
    out.denied = count_u64(denied.len());

    let rest = rest.into_iter().filter(|m| !denied.contains(&m.key));
    let (queue, count_only): (Vec<&Merged>, Vec<&Merged>) = match gate {
        QueueGate::Open => (rest.collect(), Vec::new()),
        QueueGate::PriorityOnly => rest.partition(|m| m.priority),
        QueueGate::Closed => (Vec::new(), rest.collect()),
    };
    if !queue.is_empty() {
        let (keys, counts) = columns(queue.iter().copied());
        let inserted: Vec<bool> = sqlx::query_scalar(OBSERVE_QUEUE_SQL)
            .bind(&keys)
            .bind(&counts)
            .fetch_all(&mut *conn)
            .await?;
        out.queued = count_u64(inserted.iter().filter(|b| **b).count());
    }
    if !count_only.is_empty() {
        let (keys, counts) = columns(count_only.iter().copied());
        let existing: Vec<Vec<u8>> = sqlx::query_scalar(OBSERVE_COUNT_ONLY_SQL)
            .bind(&keys)
            .bind(&counts)
            .fetch_all(&mut *conn)
            .await?;
        out.dropped = count_u64(count_only.len().saturating_sub(existing.len()));
    }
    Ok(out)
}

/// The keys and counts of `entries`, as bind arrays. The keys are borrowed:
/// each statement needs only a pointer per key, not a cloned heap buffer.
fn columns<'a>(entries: impl Iterator<Item = &'a Merged>) -> (Vec<&'a [u8]>, Vec<i64>) {
    entries.map(|m| (m.key.as_slice(), m.n)).unzip()
}

fn validate_torrent(t: &NewTorrent) -> Result<()> {
    check_len("name", &t.name, 1, MAX_NAME_CHARS)?;
    if t.files.len() > MAX_STORED_FILES {
        return Err(StoreError::Invalid(format!(
            "{} file rows exceed the limit of {MAX_STORED_FILES}",
            t.files.len()
        )));
    }
    for f in &t.files {
        check_len("file path", &f.path, 0, MAX_PATH_CHARS)?;
        to_i64("file size", f.size)?;
    }
    if t.piece_length == Some(0) {
        return Err(StoreError::Invalid("piece_length must be positive".into()));
    }
    Ok(())
}

/// The body of [`Store::deny`], for use inside a larger transaction.
pub(crate) async fn deny_in_tx(
    conn: &mut PgConnection,
    key: &[u8],
    reason: DenyReason,
    note: Option<&str>,
    actor: &str,
) -> Result<DenyOutcome> {
    check_key_len(key)?;
    check_note(note)?;
    check_actor(actor)?;
    let p = prefix(key);

    lock_change_shared(conn).await?;
    lock_keys(conn, &[p]).await?;

    let newly_denied = sqlx::query(
        "INSERT INTO denylist (key, reason, note, created_by) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(key)
    .bind(reason.as_str())
    .bind(note)
    .bind(actor)
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0;

    let ids: Vec<i64> = sqlx::query_scalar(TOMBSTONE_SQL)
        .bind(p)
        .fetch_all(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM pending WHERE dht_key = $1")
        .bind(p)
        .execute(&mut *conn)
        .await?;

    let detail = serde_json::json!({
        "reason": reason.as_str(),
        "note": note,
        "newly_denied": newly_denied,
        "tombstoned_ids": ids,
    });
    write_audit(conn, actor, "deny", &hex_of(key), &detail).await?;
    if newly_denied {
        bump_daily(conn, DailyCounter::Blocked, 1).await?;
    }
    Ok(DenyOutcome {
        newly_denied,
        tombstoned: count_u64(ids.len()),
    })
}
