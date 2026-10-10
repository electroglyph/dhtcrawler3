//! Indexer operations: the change feed. Both methods work for the
//! `dc4_indexer` user.

use std::time::Duration;

use crate::types::IndexRow;
use crate::{
    CHANGE_LOCK_KEY, FEED_PAGE_MAX_BYTES, HWM_LOCK_TIMEOUT, MAX_FEED_PAGE, Result,
    SQLSTATE_LOCK_NOT_AVAILABLE, Store, get, has_sqlstate,
};

/// Rows of a [`Store::changes_since`] page: at most `$3` rows with
/// `$1 < change_seq <= $2`, stopping once the earlier rows hold `$4` bytes of
/// text (the first row is always included). The sizes come from the JSON text
/// of `files`, which is at least as long as the joined paths. Only the rows
/// kept are joined back and have their paths assembled.
///
/// Hard-purged tombstones join the same stream: `purge_tombstoned` records
/// one `purged_torrents` row per deleted id with its own `change_seq` stamp,
/// served back below as a `visible = false` row with dummy payload columns
/// (the indexer deletes by id, like for tombstones).
const CHANGES_SQL: &str = concat!(
    "\
WITH page AS (
    (SELECT t.id, t.change_seq, octet_length(t.name)::bigint + octet_length(t.files::text) AS bytes
       FROM torrents t
      WHERE t.change_seq > $1 AND t.change_seq <= $2)
    UNION ALL
    (SELECT p.torrent_id AS id, p.purged_seq AS change_seq, 0 AS bytes
       FROM purged_torrents p
      WHERE p.purged_seq > $1 AND p.purged_seq <= $2)
    ORDER BY change_seq
    LIMIT $3
), sized AS (
    SELECT p.id, p.change_seq,
           coalesce(sum(p.bytes) OVER (ORDER BY p.change_seq
                                       ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0)
               AS before
      FROM page p
)
SELECT s.id, s.change_seq,
       coalesce(t.dht_key, decode('0000000000000000000000000000000000000000', 'hex')) AS dht_key,
       t.info_hash_v1, t.info_hash_v2,
       coalesce(t.name, '') AS name, coalesce(x.files_text, '') AS files_text,
       coalesce(t.total_size, 0) AS total_size, coalesce(t.file_count, 0) AS file_count,
       coalesce(t.first_seen_at, 'epoch'::timestamptz) AS first_seen_at,
       coalesce(t.seen_count, 0) AS seen_count,
       t.last_scraped_at, t.seeders_est, ",
    "(t.id IS NOT NULL AND t.deleted_at IS NULL)",
    " AS visible
  FROM sized s
  LEFT JOIN torrents t ON t.id = s.id
 LEFT JOIN LATERAL (
       SELECT coalesce(string_agg(f.e ->> 'p', E'\\n' ORDER BY f.o), '') AS files_text
         FROM jsonb_array_elements(coalesce(t.files, '[]')) WITH ORDINALITY AS f(e, o)) x ON true
 WHERE s.before < $4
 ORDER BY s.change_seq"
);

impl Store {
    /// A change-feed position at or below which every `change_seq` belongs to a
    /// finished transaction, or `None` when a writer held the change lock for
    /// longer than [`HWM_LOCK_TIMEOUT`]. On `None`, keep the previous mark and
    /// try again later.
    ///
    /// Takes the change lock exclusively for an instant, which waits for every
    /// writer holding it in shared mode, then reads the sequence's last value
    /// (0 when the sequence has never been used).
    pub async fn high_water_mark(&self) -> Result<Option<i64>> {
        self.high_water_mark_within(HWM_LOCK_TIMEOUT).await
    }

    /// [`Store::high_water_mark`] with an explicit lock timeout (at least
    /// 1 ms; PostgreSQL reads 0 as "wait forever").
    pub(crate) async fn high_water_mark_within(&self, timeout: Duration) -> Result<Option<i64>> {
        let mut tx = self.pool.begin().await?;
        // SET LOCAL lock_timeout, with the value bound as a parameter.
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(format!("{}ms", timeout.as_millis().max(1)))
            .execute(&mut *tx)
            .await?;
        let locked = sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CHANGE_LOCK_KEY)
            .execute(&mut *tx)
            .await;
        match locked {
            Ok(_) => {}
            Err(e) if has_sqlstate(&e, SQLSTATE_LOCK_NOT_AVAILABLE) => {
                tx.rollback().await?;
                tracing::debug!("change-feed lock busy; the high-water mark stays put");
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
        let row = sqlx::query("SELECT last_value, is_called FROM change_seq")
            .fetch_one(&mut *tx)
            .await?;
        let last: i64 = get(&row, "last_value")?;
        let called: bool = get(&row, "is_called")?;
        tx.commit().await?;
        Ok(Some(if called { last } else { 0 }))
    }

    /// Rows with `after < change_seq <= upto`, ordered by `change_seq`. Rows
    /// with `visible == false` must be removed from the index.
    ///
    /// Returns at most `limit` rows (capped at [`MAX_FEED_PAGE`]) and stops
    /// early once the rows hold [`FEED_PAGE_MAX_BYTES`] of text. A page is
    /// never empty while rows remain, but a short page does not prove that the
    /// range is drained: continue from the last row's `change_seq`.
    pub async fn changes_since(&self, after: i64, upto: i64, limit: i64) -> Result<Vec<IndexRow>> {
        self.changes_within(after, upto, limit, FEED_PAGE_MAX_BYTES)
            .await
    }

    /// [`Store::changes_since`] with an explicit byte budget.
    pub(crate) async fn changes_within(
        &self,
        after: i64,
        upto: i64,
        limit: i64,
        max_bytes: i64,
    ) -> Result<Vec<IndexRow>> {
        if limit <= 0 || upto <= after {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(CHANGES_SQL)
            .bind(after)
            .bind(upto)
            .bind(limit.min(MAX_FEED_PAGE))
            .bind(max_bytes.max(1))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(IndexRow::from_row).collect()
    }
}
