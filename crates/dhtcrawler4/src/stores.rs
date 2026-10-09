//! The store operations the pipeline uses, as traits, so the crawl and index
//! loops can run against [`dc3_store::Store`] or an in-memory stand-in
//! ([`crate::memstore::MemoryStore`]).

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Utc};
use dc3_core::DhtKey;
use dc3_store::{
    IndexRow, NewTorrent, Observation, ObserveOutcome, PendingItem, RemovalCooldown, Result,
    ScrapeItem, Store,
};

/// What the crawl role needs from the database (the `dc3_crawler` user).
pub trait CrawlStore: Clone + Send + Sync + 'static {
    /// See [`Store::observe`].
    fn observe(
        &self,
        batch: &[Observation],
        max_pending: i64,
    ) -> impl Future<Output = Result<ObserveOutcome>> + Send;
    /// See [`Store::claim`].
    fn claim(
        &self,
        n: i64,
        lease: Duration,
    ) -> impl Future<Output = Result<Vec<PendingItem>>> + Send;
    /// See [`Store::renew`].
    fn renew(&self, key: &DhtKey, lease: Duration) -> impl Future<Output = Result<bool>> + Send;
    /// See [`Store::complete`].
    fn complete(&self, key: &DhtKey, t: &NewTorrent) -> impl Future<Output = Result<i64>> + Send;
    /// See [`Store::fail`].
    fn fail(&self, key: &DhtKey) -> impl Future<Output = Result<bool>> + Send;
    /// See [`Store::give_up`].
    fn give_up(&self, key: &DhtKey) -> impl Future<Output = Result<bool>> + Send;
    /// See [`Store::pending_depth`].
    fn pending_depth(&self) -> impl Future<Output = Result<i64>> + Send;
    /// See [`Store::claim_scrape_due`].
    fn claim_scrape_due(
        &self,
        limit: i64,
        live_interval: Duration,
        unknown_interval: Duration,
    ) -> impl Future<Output = Result<Vec<ScrapeItem>>> + Send;
    /// See [`Store::record_scrape`].
    fn record_scrape(
        &self,
        id: i64,
        seeders_est: Option<u32>,
        scrape_failures: u32,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// See [`Store::record_scrapes`].
    fn record_scrapes(
        &self,
        rows: &[(i64, Option<u32>, u32)],
    ) -> impl Future<Output = Result<u64>> + Send;
    /// See [`Store::tombstone_dead`].
    fn tombstone_dead(
        &self,
        id: i64,
        old_last_seen_at: DateTime<Utc>,
        old_change_seq: i64,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// See [`Store::purge_tombstoned`].
    fn purge_tombstoned(
        &self,
        grace: Duration,
        limit: i64,
    ) -> impl Future<Output = Result<u64>> + Send;
    /// See [`Store::note_fetch_estimate`].
    fn note_fetch_estimate(
        &self,
        key: &DhtKey,
        seeders_est: u32,
    ) -> impl Future<Output = Result<()>> + Send;
    /// See [`Store::trim_removed_keys`].
    fn trim_removed_keys(&self, cap: i64) -> impl Future<Output = Result<u64>> + Send;
    /// See [`Store::removed_keys_count`].
    fn removed_keys_count(&self) -> impl Future<Output = Result<i64>> + Send;
    /// See [`Store::removal_cooldowns`].
    fn removal_cooldowns(
        &self,
        keys: &[DhtKey],
        base_days: u64,
        strong_evidence: &[DhtKey],
    ) -> impl Future<Output = Result<Vec<RemovalCooldown>>> + Send;
    /// See [`Store::note_removed_sightings`].
    fn note_removed_sightings(&self, keys: &[DhtKey]) -> impl Future<Output = Result<u64>> + Send;
    /// See [`Store::refresh_scraped`].
    fn refresh_scraped(&self, keys: &[DhtKey]) -> impl Future<Output = Result<u64>> + Send;
    /// See [`Store::ping`].
    fn ping(&self) -> impl Future<Output = Result<()>> + Send;
    /// See [`Store::complete_batch`]: many keys in one transaction.
    /// The default completes one key at a time; [`Store`] overrides it.
    fn complete_batch(
        &self,
        items: &[(DhtKey, NewTorrent)],
    ) -> impl Future<Output = Result<Vec<i64>>> + Send {
        async move {
            let mut ids = Vec::with_capacity(items.len());
            for (key, t) in items {
                ids.push(self.complete(key, t).await?);
            }
            Ok(ids)
        }
    }
    /// See [`Store::fail_batch`]: many keys in one transaction.
    /// The default fails one key at a time (so duplicates bump once per
    /// call); [`Store`] merges duplicates to one bump per distinct key.
    fn fail_batch(&self, keys: &[DhtKey]) -> impl Future<Output = Result<()>> + Send {
        async move {
            for key in keys {
                self.fail(key).await?;
            }
            Ok(())
        }
    }
    /// See [`Store::give_up_batch`]: many keys in one transaction.
    /// Duplicate semantics match [`CrawlStore::fail_batch`].
    fn give_up_batch(&self, keys: &[DhtKey]) -> impl Future<Output = Result<()>> + Send {
        async move {
            for key in keys {
                self.give_up(key).await?;
            }
            Ok(())
        }
    }
    /// Pool use, if backed by one: (open connections, idle connections).
    fn pool_status(&self) -> Option<(u32, usize)> {
        None
    }
}

impl CrawlStore for Store {
    fn observe(
        &self,
        batch: &[Observation],
        max_pending: i64,
    ) -> impl Future<Output = Result<ObserveOutcome>> + Send {
        Store::observe(self, batch, max_pending)
    }

    fn claim(
        &self,
        n: i64,
        lease: Duration,
    ) -> impl Future<Output = Result<Vec<PendingItem>>> + Send {
        Store::claim(self, n, lease)
    }

    fn renew(&self, key: &DhtKey, lease: Duration) -> impl Future<Output = Result<bool>> + Send {
        Store::renew(self, key, lease)
    }

    fn complete(&self, key: &DhtKey, t: &NewTorrent) -> impl Future<Output = Result<i64>> + Send {
        Store::complete(self, key, t)
    }

    fn fail(&self, key: &DhtKey) -> impl Future<Output = Result<bool>> + Send {
        Store::fail(self, key)
    }

    fn give_up(&self, key: &DhtKey) -> impl Future<Output = Result<bool>> + Send {
        Store::give_up(self, key)
    }

    fn pending_depth(&self) -> impl Future<Output = Result<i64>> + Send {
        Store::pending_depth(self)
    }

    fn claim_scrape_due(
        &self,
        limit: i64,
        live_interval: Duration,
        unknown_interval: Duration,
    ) -> impl Future<Output = Result<Vec<ScrapeItem>>> + Send {
        Store::claim_scrape_due(self, limit, live_interval, unknown_interval)
    }

    fn record_scrape(
        &self,
        id: i64,
        seeders_est: Option<u32>,
        scrape_failures: u32,
    ) -> impl Future<Output = Result<bool>> + Send {
        Store::record_scrape(self, id, seeders_est, scrape_failures)
    }

    fn record_scrapes(
        &self,
        rows: &[(i64, Option<u32>, u32)],
    ) -> impl Future<Output = Result<u64>> + Send {
        Store::record_scrapes(self, rows)
    }

    fn tombstone_dead(
        &self,
        id: i64,
        old_last_seen_at: DateTime<Utc>,
        old_change_seq: i64,
    ) -> impl Future<Output = Result<bool>> + Send {
        Store::tombstone_dead(self, id, old_last_seen_at, old_change_seq)
    }

    fn purge_tombstoned(
        &self,
        grace: Duration,
        limit: i64,
    ) -> impl Future<Output = Result<u64>> + Send {
        Store::purge_tombstoned(self, grace, limit)
    }

    fn note_fetch_estimate(
        &self,
        key: &DhtKey,
        seeders_est: u32,
    ) -> impl Future<Output = Result<()>> + Send {
        Store::note_fetch_estimate(self, key, seeders_est)
    }

    fn trim_removed_keys(&self, cap: i64) -> impl Future<Output = Result<u64>> + Send {
        Store::trim_removed_keys(self, cap)
    }

    fn removed_keys_count(&self) -> impl Future<Output = Result<i64>> + Send {
        Store::removed_keys_count(self)
    }

    fn removal_cooldowns(
        &self,
        keys: &[DhtKey],
        base_days: u64,
        strong_evidence: &[DhtKey],
    ) -> impl Future<Output = Result<Vec<RemovalCooldown>>> + Send {
        Store::removal_cooldowns(self, keys, base_days, strong_evidence)
    }

    fn note_removed_sightings(&self, keys: &[DhtKey]) -> impl Future<Output = Result<u64>> + Send {
        Store::note_removed_sightings(self, keys)
    }

    fn refresh_scraped(&self, keys: &[DhtKey]) -> impl Future<Output = Result<u64>> + Send {
        Store::refresh_scraped(self, keys)
    }

    fn ping(&self) -> impl Future<Output = Result<()>> + Send {
        Store::ping(self)
    }

    fn complete_batch(
        &self,
        items: &[(DhtKey, NewTorrent)],
    ) -> impl Future<Output = Result<Vec<i64>>> + Send {
        Store::complete_batch(self, items)
    }

    fn fail_batch(&self, keys: &[DhtKey]) -> impl Future<Output = Result<()>> + Send {
        Store::fail_batch(self, keys)
    }

    fn give_up_batch(&self, keys: &[DhtKey]) -> impl Future<Output = Result<()>> + Send {
        Store::give_up_batch(self, keys)
    }

    fn pool_status(&self) -> Option<(u32, usize)> {
        let pool = self.pool();
        Some((pool.size(), pool.num_idle()))
    }
}

/// What the index role needs from the database (the `dc3_indexer` user).
pub trait ChangeFeed: Clone + Send + Sync + 'static {
    /// See [`Store::high_water_mark`].
    fn high_water_mark(&self) -> impl Future<Output = Result<Option<i64>>> + Send;
    /// See [`Store::changes_since`].
    fn changes_since(
        &self,
        after: i64,
        upto: i64,
        limit: i64,
    ) -> impl Future<Output = Result<Vec<IndexRow>>> + Send;
}

impl ChangeFeed for Store {
    fn high_water_mark(&self) -> impl Future<Output = Result<Option<i64>>> + Send {
        Store::high_water_mark(self)
    }

    fn changes_since(
        &self,
        after: i64,
        upto: i64,
        limit: i64,
    ) -> impl Future<Output = Result<Vec<IndexRow>>> + Send {
        Store::changes_since(self, after, upto, limit)
    }
}
