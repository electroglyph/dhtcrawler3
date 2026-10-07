//! Web operations: lookups and public statistics. Every method here
//! works for the `dc3_web` user, which can read `torrents` and
//! `stats_daily`, and nothing else.

use dc3_core::AnyKey;

use crate::types::{DailyStats, PublicStats, TorrentRecord, torrent_columns_base, visible_sql};
use crate::{
    GET_BY_KEY_MAX_PATH_BYTES, GET_MANY_MAX_FILES, MAX_DAILY_STATS_DAYS, MAX_GET_MANY,
    MAX_STORED_FILES, Result, Store, StoreError, get, live_torrents,
};

/// Visible torrents by id, in the order given, with at most `$2` file rows
/// each (the first ones in stored order).
const GET_MANY_SQL: &str = concat!(
    "SELECT ",
    torrent_columns_base!(),
    ", x.files \
     FROM unnest($1::bigint[]) WITH ORDINALITY AS u(id, ord) \
     JOIN torrents t ON t.id = u.id \
     CROSS JOIN LATERAL ( \
         SELECT coalesce(jsonb_agg(f.e ORDER BY f.o), '[]'::jsonb) AS files \
           FROM jsonb_array_elements(t.files) WITH ORDINALITY AS f(e, o) \
          WHERE f.o <= $2) x \
     WHERE ",
    visible_sql!(),
    " ORDER BY u.ord"
);

/// The file rows of `t` that [`Store::get_by_key`] returns: the first ones in
/// stored order, at most `$2` of them, whose paths hold at most `$3` bytes
/// together. `cut` is true when any row is left out.
macro_rules! bounded_files_sql {
    () => {
        " CROSS JOIN LATERAL ( \
             SELECT coalesce(jsonb_agg(c.e ORDER BY c.o) FILTER (WHERE c.keep), '[]'::jsonb) AS files, \
                    coalesce(bool_or(NOT c.keep), false) AS cut \
               FROM (SELECT f.e, f.o, \
                            f.o <= $2 AND sum(coalesce(octet_length(f.e ->> 'p'), 0)) \
                                OVER (ORDER BY f.o ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) <= $3 \
                                AS keep \
                       FROM jsonb_array_elements(t.files) WITH ORDINALITY AS f(e, o)) c) x"
    };
}

impl Store {
    /// The visible torrent with this key. A [`AnyKey::V1OrDht`] key matches
    /// the DHT key or the v1 infohash (the DHT-key match wins); a
    /// [`AnyKey::V2`] key matches the v2 infohash.
    ///
    /// The file list is bounded: the first stored rows whose paths hold at
    /// most [`GET_BY_KEY_MAX_PATH_BYTES`] together (and at most
    /// [`MAX_STORED_FILES`] rows). When rows are left out, `files_truncated`
    /// is true; `file_count` is always the full count.
    pub async fn get_by_key(&self, key: &AnyKey) -> Result<Option<TorrentRecord>> {
        let max_files = i64::try_from(MAX_STORED_FILES).unwrap_or(i64::MAX);
        let row = match key {
            AnyKey::V1OrDht(k) => {
                sqlx::query(concat!(
                    "SELECT ",
                    torrent_columns_base!(),
                    ", x.files, x.cut FROM torrents t",
                    bounded_files_sql!(),
                    " WHERE (t.dht_key = $1 OR t.info_hash_v1 = $1) AND ",
                    visible_sql!(),
                    " ORDER BY (t.dht_key = $1) DESC LIMIT 1"
                ))
                .bind(k.as_bytes().as_slice())
                .bind(max_files)
                .bind(GET_BY_KEY_MAX_PATH_BYTES)
                .fetch_optional(&self.pool)
                .await?
            }
            AnyKey::V2(h) => {
                sqlx::query(concat!(
                    "SELECT ",
                    torrent_columns_base!(),
                    ", x.files, x.cut FROM torrents t",
                    bounded_files_sql!(),
                    " WHERE t.info_hash_v2 = $1 AND ",
                    visible_sql!()
                ))
                .bind(h.as_bytes().as_slice())
                .bind(max_files)
                .bind(GET_BY_KEY_MAX_PATH_BYTES)
                .fetch_optional(&self.pool)
                .await?
            }
        };
        let Some(row) = row else {
            return Ok(None);
        };
        let mut record = TorrentRecord::from_row(&row)?;
        let cut: bool = get(&row, "cut")?;
        record.files_truncated |= cut;
        Ok(Some(record))
    }

    /// The visible torrents among `ids`, in the order of `ids` (at most
    /// [`MAX_GET_MANY`]). Missing or invisible ids are skipped.
    ///
    /// Each record holds at most [`GET_MANY_MAX_FILES`] file rows, the first
    /// in stored order; `file_count` is the full count. Use
    /// [`Store::get_by_key`] for a longer list.
    pub async fn get_many(&self, ids: &[i64]) -> Result<Vec<TorrentRecord>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if ids.len() > MAX_GET_MANY {
            return Err(StoreError::Invalid(format!(
                "at most {MAX_GET_MANY} ids per call"
            )));
        }
        let max_files = i64::try_from(GET_MANY_MAX_FILES).unwrap_or(i64::MAX);
        let rows = sqlx::query(GET_MANY_SQL)
            .bind(ids)
            .bind(max_files)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(TorrentRecord::from_row).collect()
    }

    /// Totals for the public home page. `torrents` is counted as in
    /// [`Store::stats`]; the other two are `stats_daily.fetched` for today
    /// and yesterday (UTC), 0 for a day without a row.
    pub async fn public_stats(&self) -> Result<PublicStats> {
        let torrents = live_torrents(&self.pool).await?;
        let row = sqlx::query(
            "SELECT coalesce((SELECT s.fetched FROM stats_daily s WHERE s.day = d.today), 0) AS today, \
                    coalesce((SELECT s.fetched FROM stats_daily s WHERE s.day = d.today - 1), 0) AS yesterday \
               FROM (SELECT (now() AT TIME ZONE 'UTC')::date AS today) d",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(PublicStats {
            torrents,
            added_today: get(&row, "today")?,
            added_yesterday: get(&row, "yesterday")?,
        })
    }

    /// Counters for the last `days` UTC days (today included, capped at
    /// [`MAX_DAILY_STATS_DAYS`]), oldest first. Days without activity are
    /// omitted.
    pub async fn daily_stats(&self, days: u32) -> Result<Vec<DailyStats>> {
        let days = i32::try_from(days.min(MAX_DAILY_STATS_DAYS)).unwrap_or(0);
        if days == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT day, discovered, fetched, fetch_failed FROM stats_daily \
             WHERE day > (now() AT TIME ZONE 'UTC')::date - $1 ORDER BY day",
        )
        .bind(days)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(DailyStats::from_row).collect()
    }
}
