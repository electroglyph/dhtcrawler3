//! Crawler operations: observation, the lease queue, completion and the
//! scrape bookkeeping. Every method here needs the `dc4_crawler` user (or the
//! owner).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use dc4_core::DhtKey;
use sqlx::PgConnection;

use crate::types::{
    NewTorrent, Observation, ObserveOutcome, PendingItem, RemovalCooldown, ScrapeItem,
    files_to_json,
};
use crate::{
    CountedTable, DailyCounter, EXACT_COUNT_THRESHOLD, FAIL_BASE_BACKOFF, FAIL_MAX_BACKOFF,
    MAX_CLAIM, MAX_COMPLETE_BATCH, MAX_FETCH_ATTEMPTS, MAX_NAME_CHARS, MAX_OBSERVE_BATCH,
    MAX_PATH_CHARS, MAX_STORED_FILES, OBSERVE_CHUNK, Result, Store, StoreError, bump_daily,
    check_key_len, check_len, count_u64, estimated_rows, get, lock_change_shared, lock_keys,
    prefix, secs, to_i64,
};

/// Bumps `seen_count` of live torrents stored under the given DHT keys.
/// `change_seq` moves only when floor(log2(seen_count)) changes: for
/// positive a and b the highest set bit is equal exactly when
/// (a XOR b) < (a AND b). Tombstoned rows are left alone: only a fetch
/// revives those. The addition saturates at the bigint maximum instead of
/// aborting the batch. The `sums` read locks its rows (`FOR UPDATE`), so a
/// concurrent writer is re-read after it commits instead of being
/// overwritten with a stale sum (READ COMMITTED re-evaluation).
const OBSERVE_KNOWN_SQL: &str = "\
WITH input(k, n) AS (SELECT * FROM unnest($1::bytea[], $2::bigint[])),
sums AS (
  SELECT i.k AS k,
         CASE WHEN t.seen_count > 9223372036854775807 - i.n
              THEN 9223372036854775807
              ELSE t.seen_count + i.n END AS new_seen,
         t.seen_count AS old_seen
    FROM input i JOIN torrents t ON t.dht_key = i.k AND t.deleted_at IS NULL
   FOR UPDATE
)
UPDATE torrents t
   SET seen_count = s.new_seen,
       last_seen_at = now(),
       change_seq = CASE WHEN (s.old_seen # s.new_seen) > (s.old_seen & s.new_seen)
                         THEN nextval('change_seq') ELSE t.change_seq END
  FROM sums s
 WHERE t.dht_key = s.k AND t.deleted_at IS NULL
RETURNING s.k";

/// Same, for keys that are another name of a stored torrent (its v1 infohash or
/// truncated v2 infohash, while the row is stored under a different DHT key).
/// Tombstoned rows are left alone, as above; the addition saturates, as above.
const OBSERVE_ALIAS_SQL: &str = "\
WITH input(k, n) AS (SELECT * FROM unnest($1::bytea[], $2::bigint[])),
sums AS (
  SELECT i.k AS k,
         CASE WHEN t.seen_count > 9223372036854775807 - i.n
              THEN 9223372036854775807
              ELSE t.seen_count + i.n END AS new_seen,
         t.seen_count AS old_seen
    FROM input i JOIN torrents t
      ON (t.info_hash_v1 = i.k OR substring(t.info_hash_v2 FROM 1 FOR 20) = i.k)
     AND t.deleted_at IS NULL
   FOR UPDATE
)
UPDATE torrents t
   SET seen_count = s.new_seen,
       last_seen_at = now(),
       change_seq = CASE WHEN (s.old_seen # s.new_seen) > (s.old_seen & s.new_seen)
                         THEN nextval('change_seq') ELSE t.change_seq END
  FROM sums s
 WHERE (t.info_hash_v1 = s.k OR substring(t.info_hash_v2 FROM 1 FOR 20) = s.k)
   AND t.deleted_at IS NULL
RETURNING s.k";

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
ON CONFLICT (dht_key) DO UPDATE
   SET seen_count = CASE WHEN p.seen_count > 9223372036854775807 - EXCLUDED.seen_count
                         THEN 9223372036854775807
                         ELSE p.seen_count + EXCLUDED.seen_count END
RETURNING (xmax = 0) AS inserted";

/// Counts sightings of keys already queued; queues nothing (the queue is full).
/// The addition saturates at the bigint maximum instead of aborting the batch.
const OBSERVE_COUNT_ONLY_SQL: &str = "\
UPDATE pending p
   SET seen_count = CASE WHEN p.seen_count > 9223372036854775807 - i.n
                         THEN 9223372036854775807
                         ELSE p.seen_count + i.n END
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
        ORDER BY q.attempts ASC, q.seeders_est DESC NULLS LAST, q.next_attempt_at
        LIMIT $1
          FOR UPDATE SKIP LOCKED)
RETURNING p.dht_key, p.attempts, p.seen_count, p.seeders_est";

/// Live-first half of the step-2 claim: identical to [`CLAIM_SQL`] plus
/// `seeders_est > 0` (`Some(0)` is measured dead, NULL is unscraped —
/// neither is live). Shares the `pending_claim` index as an `Index Cond`
/// (repro-proven, no migration).
const CLAIM_LIVE_SQL: &str = "\
UPDATE pending p
   SET lease_until = now() + make_interval(secs => $2)
 WHERE p.dht_key IN (
       SELECT q.dht_key FROM pending q
        WHERE NOT q.gave_up
          AND q.next_attempt_at <= now()
          AND (q.lease_until IS NULL OR q.lease_until < now())
          AND q.seeders_est > 0
        ORDER BY q.attempts ASC, q.seeders_est DESC NULLS LAST, q.next_attempt_at
        LIMIT $1
          FOR UPDATE SKIP LOCKED)
RETURNING p.dht_key, p.attempts, p.seen_count, p.seeders_est";

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
 WHERE dht_key = $1 AND NOT gave_up
RETURNING gave_up";

const GIVE_UP_SQL: &str = "\
UPDATE pending
   SET attempts = attempts + 1,
       last_attempt_at = now(),
       lease_until = NULL,
       gave_up = true
 WHERE dht_key = $1 AND NOT gave_up
RETURNING gave_up";

/// [`Store::fail_batch`]: one transaction for many keys. The backoff math is
/// identical to `FAIL_SQL`; matched keys come back for the daily count.
/// Already-gave-up rows are skipped, as in `FAIL_SQL`.
const FAIL_BATCH_SQL: &str = "UPDATE pending SET attempts = attempts + 1, last_attempt_at = now(), lease_until = NULL, next_attempt_at = now() + least(make_interval(secs => $2) * power(2, least(attempts, 30)), make_interval(secs => $3)), gave_up = attempts + 1 >= $4 WHERE dht_key = ANY($1::bytea[]) AND NOT gave_up RETURNING dht_key";

/// [`Store::give_up_batch`]: one transaction for many keys.
/// Already-gave-up rows are skipped, as in `GIVE_UP_SQL`.
const GIVE_UP_BATCH_SQL: &str = "UPDATE pending SET attempts = attempts + 1, last_attempt_at = now(), lease_until = NULL, gave_up = true WHERE dht_key = ANY($1::bytea[]) AND NOT gave_up RETURNING dht_key";

const TORRENT_UPDATE_SQL: &str = "\
UPDATE torrents
   SET info_hash_v1 = CASE WHEN $2 THEN info_hash_v1 ELSE $3 END,
       info_hash_v2 = CASE WHEN $4 THEN info_hash_v2 ELSE $5 END,
       name = $6, total_size = $7, file_count = $8, files = $9,
       files_truncated = $10, piece_length = $11,
       seen_count = CASE WHEN seen_count > 9223372036854775807 - $12
                         THEN 9223372036854775807
                         ELSE seen_count + $12 END,
       last_seen_at = now(),
       deleted_at = NULL,
       -- A duplicate completion (same content columns) is liveness only:
       -- keep change_seq so the indexer is not churned. All expressions
       -- below read the pre-update row, so this compares old vs new content.
       change_seq = CASE WHEN (info_hash_v1, info_hash_v2, name, total_size,
                               file_count, files, files_truncated, piece_length)
                              IS DISTINCT FROM
                              (CASE WHEN $2 THEN info_hash_v1 ELSE $3 END,
                               CASE WHEN $4 THEN info_hash_v2 ELSE $5 END,
                               $6, $7, $8, $9, $10, $11)
                         THEN nextval('change_seq') ELSE change_seq END
 WHERE id = $1";

const TORRENT_INSERT_SQL: &str = "\
INSERT INTO torrents (dht_key, info_hash_v1, info_hash_v2, name, total_size, file_count, files,
                      files_truncated, piece_length, seen_count, first_seen_at, change_seq)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, coalesce($11, now()), nextval('change_seq'))
RETURNING id";

/// Claims rows due for a BEP 33 scrape and stamps the claim (`$2`/`$3` are
/// the unknown/live intervals in seconds). Never-scraped rows
/// (`last_scraped_at IS NULL`) are always due; unaware rows
/// (`seeders_est IS NULL`, scraped before) use the unknown interval so they
/// are re-polled less often; rows with an estimate use the live interval.
/// The live clause is gated on `seeders_est IS NOT NULL`, else it would also
/// match unaware rows and the unknown interval would never apply.
///
/// Rows come back oldest-scrape first (never-scraped first): the inner
/// pick selects the due set in that order and the outer join re-applies it,
/// since an `UPDATE ... RETURNING` on its own promises no order.
const CLAIM_SCRAPE_SQL: &str = "\
WITH picked AS (
  SELECT s.id, s.last_scraped_at AS old_scraped FROM torrents s
   WHERE s.deleted_at IS NULL
     AND (s.last_scraped_at IS NULL
          OR (s.seeders_est IS NULL
              AND s.last_scraped_at IS NOT NULL
              AND s.last_scraped_at < now() - make_interval(secs => $2))
          OR (s.seeders_est IS NOT NULL
              AND s.last_scraped_at < now() - make_interval(secs => $3)))
   ORDER BY s.last_scraped_at ASC NULLS FIRST, s.id
   LIMIT $1
     FOR UPDATE SKIP LOCKED),
updated AS (
  UPDATE torrents t
     SET last_scraped_at = now()
    FROM picked
   WHERE t.id = picked.id
  RETURNING t.id, t.dht_key, t.seeders_est, t.scrape_failures, t.last_seen_at, t.change_seq
)
SELECT u.id, u.dht_key, u.seeders_est, u.scrape_failures, u.last_seen_at, u.change_seq
  FROM updated u JOIN picked p ON p.id = u.id
 ORDER BY p.old_scraped ASC NULLS FIRST, p.id";

/// Stats-only scrape write: no `change_seq` bump (the indexer must not see
/// churn) and no `files` touch (which would fire `torrents_files_shape`).
/// Tombstoned rows are left alone: a delayed scrape must not resurrect
/// stats on a dead row.
const RECORD_SCRAPE_SQL: &str = "\
UPDATE torrents
   SET seeders_est = $2, scrape_failures = $3, last_scraped_at = now()
 WHERE id = $1 AND deleted_at IS NULL";

/// Batched stats-only scrape write (win 7): one statement for a whole
/// scrape batch, same column set as [`RECORD_SCRAPE_SQL`] — no `change_seq`
/// bump, no `files` touch, and tombstoned rows left alone.
const RECORD_SCRAPES_SQL: &str = "\
UPDATE torrents AS t
   SET seeders_est = i.est, scrape_failures = i.failures, last_scraped_at = now()
  FROM unnest($1::bigint[], $2::integer[], $3::integer[]) AS i(id, est, failures)
 WHERE t.id = i.id AND t.deleted_at IS NULL";

/// Conditional scrape tombstone: wipes name/files but only when the row is
/// still exactly as claimed, so a concurrent fetch is never clobbered. The
/// scrape scheduling columns go too: a revived row must not inherit a stale
/// estimate, failure count or last-scraped stamp.
const TOMBSTONE_DEAD_SQL: &str = "\
UPDATE torrents
   SET name = '', files = '[]'::jsonb, files_truncated = false,
       seeders_est = NULL, scrape_failures = 0, last_scraped_at = NULL,
       deleted_at = coalesce(deleted_at, now()),
       change_seq = nextval('change_seq')
 WHERE id = $1 AND deleted_at IS NULL AND last_seen_at = $2 AND change_seq = $3";

/// Deletes tombstones older than the grace, biggest first, so each sweep
/// frees the most disk (win 5). Returns the number removed. The grace lets
/// the indexer observably drop the document first (it sees `visible=false`
/// through the change feed).
///
/// Every deleted id is also recorded in `purged_torrents` with its own
/// `change_seq` stamp, which the change feed serves back as a `visible=false`
/// row: an indexer lagging past the grace still learns the delete instead of
/// serving the document forever. `ON CONFLICT DO NOTHING` keeps concurrent
/// sweeps benign (the loser records nothing and deletes nothing).
const PURGE_TOMBSTONED_SQL: &str = "\
WITH doomed AS (
  SELECT t.id FROM torrents t
   WHERE t.deleted_at IS NOT NULL
     AND t.deleted_at < now() - make_interval(secs => $1)
   ORDER BY t.total_size DESC
   LIMIT $2
   FOR UPDATE
),
fed AS (
  INSERT INTO purged_torrents (torrent_id, purged_seq)
  SELECT id, nextval('change_seq') FROM doomed
  ON CONFLICT DO NOTHING
  RETURNING torrent_id
)
DELETE FROM torrents WHERE id IN (SELECT torrent_id FROM fed) AND deleted_at IS NOT NULL";

/// Purge-feed entries older than this are pruned by [`Store::purge_tombstoned`].
/// An indexer resuming from a checkpoint older than that must re-sync from
/// scratch.
const PURGED_FEED_RETENTION_SECS: i64 = 90 * 24 * 3600;

/// Deletes purge-feed entries older than [`PURGED_FEED_RETENTION_SECS`].
const PRUNE_PURGED_SQL: &str = "\
DELETE FROM purged_torrents WHERE purged_at < now() - make_interval(secs => $1)";

/// Upserts removal memory: a new row starts at one consecutive removal, an
/// existing one escalates while its sighting counter restarts.
const NOTE_REMOVAL_SQL: &str = "\
INSERT INTO removed_keys (key, removed_at, removals, sightings)
VALUES ($1, now(), 1, 0)
ON CONFLICT (key) DO UPDATE
   SET removed_at = now(),
       removals = removed_keys.removals + 1,
       sightings = 0";

impl Store {
    /// Records one batch of sightings in one transaction.
    ///
    /// * Keys of stored torrents get `seen_count += sightings` and
    ///   `last_seen_at = now()`; `change_seq` moves only when
    ///   floor(log2(seen_count)) changes.
    /// * Other keys add their sightings to
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
        if merged.len() > MAX_OBSERVE_BATCH {
            return Err(StoreError::Invalid(format!(
                "observe batch of {} exceeds the limit of {MAX_OBSERVE_BATCH}",
                merged.len()
            )));
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
        self.claim_with(false, n, lease).await
    }

    /// Leases up to `n` due queue items with `seeders_est > 0` for `lease`:
    /// the live-first half of the step-2 claim. Same ordering as
    /// [`Store::claim`]; keys it leases are excluded from a follow-up
    /// [`Store::claim`] by their fresh lease, so the pair never double-leases.
    pub async fn claim_live(&self, n: i64, lease: Duration) -> Result<Vec<PendingItem>> {
        self.claim_with(true, n, lease).await
    }

    async fn claim_with(
        &self,
        live_only: bool,
        n: i64,
        lease: Duration,
    ) -> Result<Vec<PendingItem>> {
        if n <= 0 {
            return Ok(Vec::new());
        }
        let sql = if live_only { CLAIM_LIVE_SQL } else { CLAIM_SQL };
        let rows = sqlx::query(sql)
            .bind(n.min(MAX_CLAIM))
            .bind(secs(lease))
            .fetch_all(&self.pool)
            .await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let attempts: i32 = get(row, "attempts")?;
            let seen: i64 = get(row, "seen_count")?;
            let est: Option<i32> = get(row, "seeders_est")?;
            items.push(PendingItem {
                dht_key: crate::types::dht_key_col(row, "dht_key")?,
                attempts: u32::try_from(attempts)
                    .map_err(|_| StoreError::Corrupt(format!("attempts {attempts}")))?,
                seen_count: crate::to_u64("seen_count", seen)?,
                seeders_est: est
                    .map(|e| {
                        u32::try_from(e)
                            .map_err(|_| StoreError::Corrupt(format!("negative seeders_est: {e}")))
                    })
                    .transpose()?,
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
        // The estimate is small (possibly stale after a bulk load without
        // ANALYZE), so count exactly instead of trusting it: a stale-small
        // estimate with a huge real depth must not read as room. This scan
        // stops at `max_pending` rows; it only runs when `max_pending`
        // exceeds the threshold cap, i.e. on unusually large queue caps.
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
    /// tombstoned row is restored when the key is fetched again.
    /// Calling it again is harmless.
    ///
    /// Returns the torrent id.
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
        // A successful fetch is positive liveness proof: clear removal
        // memory for every name of the torrent, so a resurgent torrent is
        // not penalised forever. Clearing only the fetched key would leave
        // the stored key's cooldown behind when the two differ (aliases).
        {
            let mut clear: Vec<&[u8]> = vec![key.as_bytes()];
            for (_, dk, r1, r2) in &existing {
                clear.push(dk.as_slice());
                if let Some(r1) = r1 {
                    clear.push(r1.as_slice());
                }
                if let Some(r2) = r2 {
                    clear.push(prefix(r2));
                }
            }
            if let Some(k) = &v1 {
                clear.push(k.as_slice());
            }
            if let Some(k) = &v2_prefix {
                clear.push(k.as_bytes());
            }
            clear.sort_unstable();
            clear.dedup();
            sqlx::query("DELETE FROM removed_keys WHERE key = ANY($1::bytea[])")
                .bind(&clear)
                .execute(&mut *tx)
                .await?;
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
    /// the key has now given up; false when no queue row exists or the key
    /// already gave up (a second failure changes nothing and counts nothing).
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
    /// once. Releases the lease. Returns false when no queue row exists or
    /// the key already gave up (a second call changes nothing).
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

    /// Completes many fetched torrents in one transaction.
    ///
    /// Per-key semantics match [`Store::complete`]: the same pending-delete,
    /// alias lookup, removal-memory clearing and insert-or-update sequence
    /// runs for each item in order, so an alias stored earlier in the batch
    /// is visible to later items exactly as if it had committed first. One
    /// change-feed lock, one `stats_daily` bump and one commit cover the
    /// whole batch.
    ///
    /// Validation runs up front: any invalid item aborts the batch with
    /// nothing written. Callers fall back to [`Store::complete`] per key to
    /// isolate it.
    pub async fn complete_batch(&self, items: &[(DhtKey, NewTorrent)]) -> Result<Vec<i64>> {
        /// One validated, serialized torrent awaiting its statements.
        struct Prepared {
            key: DhtKey,
            name: String,
            files: serde_json::Value,
            total_size: i64,
            file_count: i64,
            files_truncated: bool,
            piece_length: Option<i64>,
            v1: Option<Vec<u8>>,
            v2: Option<Vec<u8>>,
            v2_prefix: Option<DhtKey>,
        }
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if items.len() > MAX_COMPLETE_BATCH {
            return Err(StoreError::Invalid(format!(
                "complete batch of {} exceeds the limit of {MAX_COMPLETE_BATCH}",
                items.len()
            )));
        }
        let mut prepared = Vec::with_capacity(items.len());
        for (key, t) in items {
            validate_torrent(t)?;
            if t.dht_key != *key {
                return Err(StoreError::Invalid(
                    "NewTorrent.dht_key differs from the completed key".into(),
                ));
            }
            prepared.push(Prepared {
                key: *key,
                name: t.name.clone(),
                files: files_to_json(&t.files)?,
                total_size: to_i64("total_size", t.total_size)?,
                file_count: to_i64("file_count", t.file_count)?,
                files_truncated: t.files_truncated,
                piece_length: t
                    .piece_length
                    .map(|p| to_i64("piece_length", p))
                    .transpose()?,
                v1: t.info_hash_v1.map(|k| k.0.to_vec()),
                v2: t.info_hash_v2.map(|h| h.0.to_vec()),
                v2_prefix: t.info_hash_v2.map(|h| h.truncated()),
            });
        }

        let mut tx = self.pool.begin().await?;
        lock_change_shared(&mut tx).await?;
        {
            let mut lock_set: Vec<&[u8]> = Vec::new();
            for p in &prepared {
                lock_set.push(p.key.as_bytes().as_slice());
                if let Some(k) = &p.v1 {
                    lock_set.push(k.as_slice());
                }
                if let Some(k) = &p.v2_prefix {
                    lock_set.push(k.as_bytes().as_slice());
                }
            }
            lock_keys(&mut tx, &lock_set).await?;
        }

        let mut ids = Vec::with_capacity(prepared.len());
        let mut counted = 0i64;
        for p in &prepared {
            let pending = sqlx::query(
                "DELETE FROM pending WHERE dht_key = $1 RETURNING seen_count, discovered_at",
            )
            .bind(p.key.as_bytes().as_slice())
            .fetch_optional(&mut *tx)
            .await?;
            let (add_seen, discovered_at) = match &pending {
                Some(row) => (
                    get::<i64>(row, "seen_count")?,
                    Some(get::<chrono::DateTime<chrono::Utc>>(row, "discovered_at")?),
                ),
                None => (0, None),
            };

            let rows = sqlx::query(
                "SELECT id, dht_key, info_hash_v1, info_hash_v2 FROM torrents WHERE dht_key = $1 OR info_hash_v1 = $2 OR info_hash_v2 = $3 ORDER BY id FOR UPDATE",
            )
            .bind(p.key.as_bytes().as_slice())
            .bind(p.v1.as_deref())
            .bind(p.v2.as_deref())
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
            // A successful fetch is positive liveness proof: clear removal
            // memory for every name of the torrent (see [`Store::complete`]).
            {
                let mut clear: Vec<&[u8]> = vec![p.key.as_bytes().as_slice()];
                for (_, dk, r1, r2) in &existing {
                    clear.push(dk.as_slice());
                    if let Some(r1) = r1 {
                        clear.push(r1.as_slice());
                    }
                    if let Some(r2) = r2 {
                        clear.push(prefix(r2));
                    }
                }
                if let Some(k) = &p.v1 {
                    clear.push(k.as_slice());
                }
                if let Some(k) = &p.v2_prefix {
                    clear.push(k.as_bytes().as_slice());
                }
                clear.sort_unstable();
                clear.dedup();
                sqlx::query("DELETE FROM removed_keys WHERE key = ANY($1::bytea[])")
                    .bind(&clear)
                    .execute(&mut *tx)
                    .await?;
            }
            let target = existing
                .iter()
                .find(|(_, dk, _, _)| dk.as_slice() == p.key.as_bytes().as_slice())
                .or_else(|| existing.first());

            let (id, item_counted) = match target {
                Some((id, _, _, _)) => {
                    let id = *id;
                    // Keep the stored hash if another row already owns the new one.
                    let conflict_v1 = p.v1.is_some()
                        && existing.iter().any(|(o, _, r1, _)| *o != id && *r1 == p.v1);
                    let conflict_v2 = p.v2.is_some()
                        && existing.iter().any(|(o, _, _, r2)| *o != id && *r2 == p.v2);
                    sqlx::query(TORRENT_UPDATE_SQL)
                        .bind(id)
                        .bind(conflict_v1)
                        .bind(p.v1.as_deref())
                        .bind(conflict_v2)
                        .bind(p.v2.as_deref())
                        .bind(p.name.as_str())
                        .bind(p.total_size)
                        .bind(p.file_count)
                        .bind(&p.files)
                        .bind(p.files_truncated)
                        .bind(p.piece_length)
                        .bind(add_seen)
                        .execute(&mut *tx)
                        .await?;
                    (id, pending.is_some())
                }
                None => {
                    let id: i64 = sqlx::query_scalar(TORRENT_INSERT_SQL)
                        .bind(p.key.as_bytes().as_slice())
                        .bind(p.v1.as_deref())
                        .bind(p.v2.as_deref())
                        .bind(p.name.as_str())
                        .bind(p.total_size)
                        .bind(p.file_count)
                        .bind(&p.files)
                        .bind(p.files_truncated)
                        .bind(p.piece_length)
                        .bind(add_seen.max(1))
                        .bind(discovered_at)
                        .fetch_one(&mut *tx)
                        .await?;
                    (id, true)
                }
            };
            if item_counted {
                counted = counted.saturating_add(1);
            }
            ids.push(id);
        }
        bump_daily(&mut tx, DailyCounter::Fetched, counted).await?;
        tx.commit().await?;
        Ok(ids)
    }

    /// Records failed fetches for many keys in one transaction.
    ///
    /// The backoff math matches [`Store::fail`]; only the round trips are
    /// shared. Duplicate keys are merged: each distinct queued key gets one
    /// attempt bump (sequential [`Store::fail`] calls would bump once per
    /// call). Callers pass distinct claimed keys, so this never triggers in
    /// the pipeline. Returns nothing: like [`Store::fail`], callers keep
    /// their own outcome and only need to know the write succeeded.
    pub async fn fail_batch(&self, keys: &[DhtKey]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut distinct: Vec<DhtKey> = keys.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        let raw: Vec<&[u8]> = distinct.iter().map(|k| k.as_bytes().as_slice()).collect();
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(FAIL_BATCH_SQL)
            .bind(&raw)
            .bind(secs(FAIL_BASE_BACKOFF))
            .bind(secs(FAIL_MAX_BACKOFF))
            .bind(MAX_FETCH_ATTEMPTS)
            .fetch_all(&mut *tx)
            .await?;
        bump_daily(
            &mut tx,
            DailyCounter::FetchFailed,
            i64::try_from(rows.len()).unwrap_or(i64::MAX),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records many never-succeed fetches in one transaction.
    /// See [`Store::give_up`]. Duplicate keys are merged, as in
    /// [`Store::fail_batch`]: one attempt bump per distinct key.
    pub async fn give_up_batch(&self, keys: &[DhtKey]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut distinct: Vec<DhtKey> = keys.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        let raw: Vec<&[u8]> = distinct.iter().map(|k| k.as_bytes().as_slice()).collect();
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(GIVE_UP_BATCH_SQL)
            .bind(&raw)
            .fetch_all(&mut *tx)
            .await?;
        bump_daily(
            &mut tx,
            DailyCounter::FetchFailed,
            i64::try_from(rows.len()).unwrap_or(i64::MAX),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes queue rows that gave up more than `older_than` ago, so their
    /// keys may be queued again. Returns the number removed. A `gave_up`
    /// row with no `last_attempt_at` (only possible via manual writes: the
    /// code paths always stamp it) is purged too — it can never satisfy an
    /// age comparison, so otherwise it would leak forever.
    pub async fn purge_gave_up(&self, older_than: Duration) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM pending WHERE gave_up AND (last_attempt_at IS NULL OR last_attempt_at < now() - make_interval(secs => $1))",
        )
        .bind(secs(older_than))
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Claims up to `limit` stored torrents due for a BEP 33 scrape, oldest
    /// scrape first (never-scraped first). A row is due when it is not
    /// tombstoned and either never scraped, or
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

    /// Records a whole scrape batch in one statement (win 7, bep33.md §12):
    /// same stats-only column set as [`Store::record_scrape`], so a batch
    /// of any size moves no feed position. Returns the rows written (one
    /// per distinct input `id` that exists). Duplicate ids in the batch are
    /// merged (last wins) so the count is deterministic.
    pub async fn record_scrapes(&self, rows: &[(i64, Option<u32>, u32)]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        // Last-wins on duplicate ids: matches what the single-statement
        // update below applies, and keeps the returned count exact.
        let mut merged: BTreeMap<i64, (Option<u32>, u32)> = BTreeMap::new();
        for (id, est, fail) in rows.iter().copied() {
            merged.insert(id, (est, fail));
        }
        let mut ids = Vec::with_capacity(merged.len());
        let mut ests = Vec::with_capacity(merged.len());
        let mut failures = Vec::with_capacity(merged.len());
        for (id, (est, fail)) in merged {
            ids.push(id);
            ests.push(
                est.map(|e| {
                    i32::try_from(e)
                        .map_err(|_| StoreError::Invalid(format!("seeders_est {e} exceeds i32")))
                })
                .transpose()?,
            );
            failures.push(
                i32::try_from(fail).map_err(|_| {
                    StoreError::Invalid(format!("scrape_failures {fail} exceeds i32"))
                })?,
            );
        }
        let n = sqlx::query(RECORD_SCRAPES_SQL)
            .bind(&ids)
            .bind(&ests)
            .bind(&failures)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n)
    }

    /// Tombstones a dead torrent and notes the removal in one transaction
    /// (a crash between the two would leave a tombstone without memory, and
    /// the key would be re-fetched on every flap).
    ///
    /// The wipe is conditional on the row being unchanged since the claim
    /// (`last_seen_at` and `change_seq` snapshots): a concurrent fetch that
    /// revived or refreshed the row wins, and this call then records nothing
    /// (returns false, without touching `removed_keys`).
    pub async fn tombstone_dead(
        &self,
        id: i64,
        old_last_seen_at: chrono::DateTime<chrono::Utc>,
        old_change_seq: i64,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        lock_change_shared(&mut tx).await?;
        let row: Option<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT dht_key, info_hash_v1, info_hash_v2 FROM torrents WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((key, v1, v2)) = row else {
            tx.rollback().await?;
            return Ok(false);
        };
        // Removal memory covers every name of the torrent, mirroring what
        // `complete` clears on fetch: otherwise a hybrid tombstoned via one
        // key is immediately re-queued via an alias with no cooldown.
        let mut names: Vec<Vec<u8>> = vec![key.clone()];
        if let Some(v1) = v1 {
            names.push(v1);
        }
        if let Some(v2) = v2 {
            names.push(prefix(&v2).to_vec());
        }
        names.sort_unstable();
        names.dedup();
        let lock_set: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
        lock_keys(&mut tx, &lock_set).await?;
        let n = sqlx::query(TOMBSTONE_DEAD_SQL)
            .bind(id)
            .bind(old_last_seen_at)
            .bind(old_change_seq)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        for name in &names {
            sqlx::query(NOTE_REMOVAL_SQL)
                .bind(name.as_slice())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Deletes up to `limit` tombstones older than `grace`, biggest
    /// first, so each sweep frees the most disk (win 5).
    /// Returns the number
    /// removed. The grace lets the indexer observably drop the document
    /// first (it sees `visible=false` through the change feed); the purge
    /// itself is also fed (one `visible=false` row per deleted id), so a
    /// lagging indexer still converges. Expired purge-feed entries are
    /// pruned (see `PURGED_FEED_RETENTION_SECS`).
    pub async fn purge_tombstoned(&self, grace: Duration, limit: i64) -> Result<u64> {
        // The purge stamps `purged_seq` from `change_seq`: hold the shared
        // change-feed lock like every other writer, so the high-water mark
        // can never read a stamp from an uncommitted purge.
        let mut tx = self.pool.begin().await?;
        lock_change_shared(&mut tx).await?;
        sqlx::query(PRUNE_PURGED_SQL)
            .bind(PURGED_FEED_RETENTION_SECS)
            .execute(&mut *tx)
            .await?;
        let res = sqlx::query(PURGE_TOMBSTONED_SQL)
            .bind(secs(grace))
            .bind(limit.max(0))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(res.rows_affected())
    }

    /// Records a seeder estimate observed for free by a fetch scrape lookup
    /// (§8 piggyback): sets `pending.seeders_est` for the next retry's
    /// liveness ordering. Best-effort by design; the caller logs failures.
    /// Unaware lookups must not call this (NULL means unscraped, 0 means
    /// measured dead).
    pub async fn note_fetch_estimate(&self, key: &DhtKey, seeders_est: u32) -> Result<()> {
        let est = i32::try_from(seeders_est)
            .map_err(|_| StoreError::Invalid(format!("seeders_est {seeders_est} exceeds i32")))?;
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
    /// matched by 20-byte prefix. `base_days` is the configured
    /// `removal_cooldown_days` (base, escalating ×~4 per repeat to the
    /// 90d cap). Strong evidence (≥ [`REMOVAL_STRONG_EVIDENCE_SIGHTINGS`]
    /// post-removal sightings) shortens the cooldown ÷4, never bypasses it.
    pub async fn removal_cooldown_remaining(&self, key: &[u8], base_days: u64) -> Result<Duration> {
        check_key_len(key)?;
        let row =
            sqlx::query("SELECT removed_at, removals, sightings FROM removed_keys WHERE key = $1")
                .bind(prefix(key))
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(Duration::ZERO);
        };
        let removed_at: chrono::DateTime<chrono::Utc> = get(&row, "removed_at")?;
        let removals: i32 = get(&row, "removals")?;
        let sightings: i32 = get(&row, "sightings")?;
        Ok(remaining_since(
            removed_at, removals, sightings, base_days, false,
        ))
    }

    /// The remaining cooldown of several keys in one round trip: one
    /// [`RemovalCooldown`] per `removed_keys` row found (remaining may
    /// be zero for expired rows). Keys are matched by 20-byte prefix.
    /// Admission uses this to skip keys still in cooldown, in batch.
    /// `base_days` is the configured `removal_cooldown_days`; keys in
    /// `strong_evidence` (e.g. announced with `seed=1` since the last
    /// flush) get the ÷4 strong-evidence shortening, like keys with
    /// enough post-removal sightings — shortening, never bypassing.
    pub async fn removal_cooldowns(
        &self,
        keys: &[DhtKey],
        base_days: u64,
        strong_evidence: &[DhtKey],
    ) -> Result<Vec<RemovalCooldown>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let raw: Vec<&[u8]> = keys.iter().map(|k| k.as_bytes().as_slice()).collect();
        let rows = sqlx::query(
            "SELECT key, removed_at, removals, sightings FROM removed_keys WHERE key = ANY($1::bytea[])",
        )
        .bind(&raw)
        .fetch_all(&self.pool)
        .await?;
        let strong: HashSet<&DhtKey> = strong_evidence.iter().collect();
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let key: Vec<u8> = get(row, "key")?;
            let removed_at: chrono::DateTime<chrono::Utc> = get(row, "removed_at")?;
            let removals: i32 = get(row, "removals")?;
            let sightings: i32 = get(row, "sightings")?;
            let key = DhtKey::from_slice(&key)
                .map_err(|e| StoreError::Corrupt(format!("removed_keys.key: {e}")))?;
            let strong = strong.contains(&key);
            out.push(RemovalCooldown {
                remaining: remaining_since(removed_at, removals, sightings, base_days, strong),
                sightings: u32::try_from(sightings.max(0)).unwrap_or(u32::MAX),
                key,
            });
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
        let res = sqlx::query(
            "UPDATE removed_keys SET sightings = sightings + 1 WHERE key = ANY($1::bytea[])",
        )
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
             WHERE deleted_at IS NULL \
               AND (dht_key = ANY($1::bytea[]) OR info_hash_v1 = ANY($1::bytea[]) \
                    OR substring(info_hash_v2 FROM 1 FOR 20) = ANY($1::bytea[]))",
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
    /// A negative cap deletes nothing: forgetting all removal memory would
    /// re-admit every cooled-down key early (fail-open).
    pub async fn trim_removed_keys(&self, cap: i64) -> Result<u64> {
        if cap < 0 {
            return Ok(0);
        }
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

/// Post-removal sightings that count as strong evidence (§4a): the
/// cooldown is shortened ÷4, never bypassed.
pub const REMOVAL_STRONG_EVIDENCE_SIGHTINGS: i32 = 3;
/// Cooldown cap, in days: repeats escalate toward but never past this.
/// A constant, not a knob (bep33.md §6).
const REMOVAL_COOLDOWN_CAP_DAYS: u32 = 90;

/// Cooldown for a key removed `removals` times in a row (§4a) with a
/// configured `base_days`: the base, then ×4, then the 90d cap.
pub(crate) fn removal_cooldown_with_base(base_days: u64, removals: i32) -> Duration {
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    // A zero base means "no cooldown" (direct-API only: validated Config
    // rejects 0); it must not be upgraded to a 1-day block.
    if base_days == 0 {
        return Duration::ZERO;
    }
    let base = DAY.saturating_mul(u32::try_from(base_days).unwrap_or(u32::MAX));
    let cap = DAY.saturating_mul(REMOVAL_COOLDOWN_CAP_DAYS);
    if removals <= 1 {
        base.min(cap)
    } else if removals == 2 {
        base.saturating_mul(4).min(cap)
    } else {
        cap
    }
}

/// Remaining cooldown of a row removed at `removed_at` with `removals`
/// consecutive removals. Strong evidence (`strong`, or at least
/// [`REMOVAL_STRONG_EVIDENCE_SIGHTINGS`] post-removal `sightings`)
/// shortens the cooldown ÷4 first — shortening, never bypassing, so a
/// lying announcer cannot force a fetch loop.
fn remaining_since(
    removed_at: chrono::DateTime<chrono::Utc>,
    removals: i32,
    sightings: i32,
    base_days: u64,
    strong: bool,
) -> Duration {
    let mut cooldown = removal_cooldown_with_base(base_days, removals);
    if strong || sightings >= REMOVAL_STRONG_EVIDENCE_SIGHTINGS {
        cooldown = cooldown.checked_div(4).unwrap_or(Duration::ZERO);
    }
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
    // Keys of tombstoned rows are not known-live: they fall through to the
    // queue below, so a resurgent torrent can be refetched (and revived by
    // completion) once admission lets it through. Only a fetch revives them;
    // this path never touches the dead row itself.
    out.known = count_u64(known.len());

    let rest: Vec<&Merged> = rest
        .into_iter()
        .filter(|m| !known.contains(&m.key))
        .collect();
    if rest.is_empty() {
        return Ok(out);
    }
    let rest = rest.into_iter();
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
