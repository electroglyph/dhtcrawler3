//! The store operations the pipeline uses, as traits, so the crawl and index
//! loops can run against [`dc3_store::Store`] or an in-memory stand-in
//! ([`crate::memstore::MemoryStore`]).

use std::future::Future;
use std::time::Duration;

use dc3_core::DhtKey;
use dc3_store::{
    DenyOutcome, DenyReason, IndexRow, NewTorrent, Observation, ObserveOutcome, PendingItem,
    Result, Store,
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
    /// See [`Store::deny`].
    fn deny(
        &self,
        key: &[u8],
        reason: DenyReason,
        note: Option<&str>,
        actor: &str,
    ) -> impl Future<Output = Result<DenyOutcome>> + Send;
    /// See [`Store::pending_depth`].
    fn pending_depth(&self) -> impl Future<Output = Result<i64>> + Send;
    /// See [`Store::ping`].
    fn ping(&self) -> impl Future<Output = Result<()>> + Send;
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

    fn deny(
        &self,
        key: &[u8],
        reason: DenyReason,
        note: Option<&str>,
        actor: &str,
    ) -> impl Future<Output = Result<DenyOutcome>> + Send {
        Store::deny(self, key, reason, note, actor)
    }

    fn pending_depth(&self) -> impl Future<Output = Result<i64>> + Send {
        Store::pending_depth(self)
    }

    fn ping(&self) -> impl Future<Output = Result<()>> + Send {
        Store::ping(self)
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
