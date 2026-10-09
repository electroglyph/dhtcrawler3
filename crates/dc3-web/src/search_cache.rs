//! Server-side cache for search index results (`cache.md`).
//!
//! Shared by HTML (`GET /search`) and JSON (`GET /api/v1/search`): one
//! entry per `(trimmed text, sort, page, per_page)` holds the raw
//! [`SearchResults`](dc3_search::SearchResults). Hits still hydrate every
//! request via `Backend::get_many()` — only the index search is skipped.
//! HTTP stays `no-store`; this is not client caching.
//!
//! Invalidation is TTL + LRU + a global [`IndexStamp`](dc3_search::IndexStamp):
//! any index commit clears the whole map on next request. Concurrent misses
//! for the same key coalesce onto one index search (§3 singleflight).
//!
//! The cache key holds query text in memory only. Nothing here logs a
//! query or labels a metric with one (R11).

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use dc3_search::{IndexStamp, SearchHandle, SearchResults, Sort};
use lru::LruCache;
use tokio::sync::OnceCell;

use crate::handlers::search::Failure;
use crate::telemetry::metric_names;

/// Default number of cached `(text, sort, page, per_page)` entries.
pub const DEFAULT_SEARCH_CACHE_ENTRIES: usize = 100;
/// Largest configurable number of cache entries (`0` disables).
pub const MAX_SEARCH_CACHE_ENTRIES: usize = 10_000;
/// Default time-to-live of a cache entry, in seconds (15 minutes).
pub const DEFAULT_SEARCH_CACHE_TTL_SECS: u64 = 900;
/// Largest configurable entry TTL, in seconds (7 days; `0` disables).
pub const MAX_SEARCH_CACHE_TTL_SECS: u64 = 604_800;

/// Checks `search_cache_size` / `search_cache_ttl_secs` ranges. Either zero
/// disables the cache; both are validated here so the binary config and
/// [`WebConfig`](crate::WebConfig) validators cannot drift.
pub fn validate_search_cache(entries: usize, ttl_secs: u64) -> Result<(), String> {
    if entries > MAX_SEARCH_CACHE_ENTRIES {
        return Err(format!(
            "web.search_cache_size must be at most {MAX_SEARCH_CACHE_ENTRIES}"
        ));
    }
    if ttl_secs > MAX_SEARCH_CACHE_TTL_SECS {
        return Err(format!(
            "web.search_cache_ttl_secs must be at most {MAX_SEARCH_CACHE_TTL_SECS}"
        ));
    }
    Ok(())
}

/// How many entries to keep and for how long. Either zero disables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchCacheConfig {
    pub entries: usize,
    pub ttl: Duration,
}

impl SearchCacheConfig {
    pub fn new(entries: usize, ttl: Duration) -> Self {
        Self { entries, ttl }
    }

    /// False when either knob is zero.
    pub fn enabled(&self) -> bool {
        self.entries > 0 && !self.ttl.is_zero()
    }
}

/// One cached query: exactly the text `execute()` searches (callers trim),
/// plus the sort and pagination that change the index query. Deliberately
/// no `Debug`: the key holds query text, which is never logged (R11).
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    pub text: String,
    pub sort: Sort,
    pub page: u32,
    pub per_page: u32,
}

/// The shared outcome of one index search: empty successes are `Ok` with
/// `hits.is_empty()` (shared by a flight, never inserted) and index
/// failures are `Err`. Shared via `Arc` so [`Failure`] never needs `Clone`.
pub(crate) type Flight = Result<(SearchResults, IndexStamp), Failure>;

struct Entry {
    results: SearchResults,
    expires_at: tokio::time::Instant,
}

struct Inner {
    /// Index version all entries were read at; a change clears the map.
    stamp: IndexStamp,
    map: LruCache<CacheKey, Entry>,
    /// One cell per concurrently missed distinct key; removed when its
    /// flight completes. The outer `Arc` lets the leader remove the entry
    /// while waiters still await the cell; the inner `Arc` shares the
    /// outcome without cloning `SearchResults`.
    inflight: HashMap<CacheKey, std::sync::Arc<OnceCell<std::sync::Arc<Flight>>>>,
}

/// The search query cache. Disabled when the config has zero entries or a
/// zero TTL: no map is allocated and the request path skips stamp reads.
pub(crate) struct SearchCache {
    inner: Option<Mutex<Inner>>,
    ttl: Duration,
}

/// What [`SearchCache::pre_search`] found under the lock.
pub(crate) enum PreSearch {
    /// A live entry; the lock is dropped and hydration proceeds.
    Hit(SearchResults),
    /// This caller runs the index search and [`SearchCache::install`]s it.
    Lead(std::sync::Arc<OnceCell<std::sync::Arc<Flight>>>),
    /// Another caller runs it; await the cell and share the outcome.
    Wait(std::sync::Arc<OnceCell<std::sync::Arc<Flight>>>),
}

impl SearchCache {
    /// Builds the cache; disabled (no map) when `config` has zero entries
    /// or a zero TTL. `initial_stamp` is the live index version the empty
    /// map starts under.
    pub(crate) fn new(config: SearchCacheConfig, initial_stamp: IndexStamp) -> Self {
        if config.ttl.is_zero() {
            return Self::disabled();
        }
        let Some(cap) = NonZeroUsize::new(config.entries) else {
            return Self::disabled();
        };
        Self {
            inner: Some(Mutex::new(Inner {
                stamp: initial_stamp,
                map: LruCache::new(cap),
                inflight: HashMap::new(),
            })),
            ttl: config.ttl,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            inner: None,
            ttl: Duration::ZERO,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_disabled(&self) -> bool {
        self.inner.is_none()
    }

    fn lock(&self) -> Option<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .as_ref()
            .map(|m| m.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Physical entry count, including lazily-expired-but-unvisited
    /// entries. Test-only; serving correctness never depends on it since
    /// lookups TTL-check.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().map(|inner| inner.map.len()).unwrap_or(0)
    }

    /// Drops all entries. The stamp is kept: entries below are always at
    /// least as fresh as it.
    #[cfg(test)]
    pub(crate) fn clear(&self) {
        if let Some(mut inner) = self.lock() {
            inner.map.clear();
            metrics::gauge!(metric_names::SEARCH_CACHE_ENTRIES).set(inner.map.len() as f64);
        }
    }

    /// The stamp check plus the TTL-checked lookup plus the singleflight
    /// get-or-create, under one lock acquisition. The guard is dropped
    /// before any `.await`. `None` when disabled.
    pub(crate) fn pre_search(&self, key: &CacheKey, search: &SearchHandle) -> Option<PreSearch> {
        let mut inner = self.lock()?;
        if !search.stamp_matches(&inner.stamp) {
            inner.stamp = search.stamp();
            inner.map.clear();
            metrics::counter!(metric_names::SEARCH_CACHE_MISSES, "reason" => "stamp").increment(1);
        } else {
            let live = inner.map.peek(key).map(|e| e.expires_at > now());
            match live {
                Some(true) => {
                    // Live: re-fetch with `get` so the hit refreshes LRU
                    // recency (never expiry). The entry cannot vanish under
                    // this lock; a miss here degrades to `absent`.
                    if let Some(entry) = inner.map.get(key) {
                        let results = entry.results.clone();
                        metrics::counter!(metric_names::SEARCH_CACHE_HITS).increment(1);
                        metrics::gauge!(metric_names::SEARCH_CACHE_ENTRIES)
                            .set(inner.map.len() as f64);
                        return Some(PreSearch::Hit(results));
                    }
                    metrics::counter!(metric_names::SEARCH_CACHE_MISSES, "reason" => "absent")
                        .increment(1);
                }
                Some(false) => {
                    // Expired: `pop`, never promote via `get`.
                    inner.map.pop(key);
                    metrics::counter!(metric_names::SEARCH_CACHE_MISSES, "reason" => "expired")
                        .increment(1);
                }
                None => {
                    metrics::counter!(metric_names::SEARCH_CACHE_MISSES, "reason" => "absent")
                        .increment(1);
                }
            }
        }
        if let Some(cell) = inner.inflight.get(key) {
            return Some(PreSearch::Wait(std::sync::Arc::clone(cell)));
        }
        let cell = std::sync::Arc::new(OnceCell::new());
        inner
            .inflight
            .insert(key.clone(), std::sync::Arc::clone(&cell));
        metrics::gauge!(metric_names::SEARCH_CACHE_ENTRIES).set(inner.map.len() as f64);
        Some(PreSearch::Lead(cell))
    }

    /// Installs a completed flight: removes the inflight cell iff it is
    /// still ours, adopts a newer post-search stamp (clearing first), and
    /// inserts only non-empty successes. Errors and empties are shared by
    /// the flight but never inserted.
    pub(crate) fn install(
        &self,
        key: &CacheKey,
        cell: &std::sync::Arc<OnceCell<std::sync::Arc<Flight>>>,
        flight: std::sync::Arc<Flight>,
    ) {
        let mut inner = match self.lock() {
            Some(inner) => inner,
            None => return,
        };
        if inner
            .inflight
            .get(key)
            .is_none_or(|c| !std::sync::Arc::ptr_eq(c, cell))
        {
            return;
        }
        inner.inflight.remove(key);
        let Ok((results, post)) = flight.as_ref() else {
            return;
        };
        if *post != inner.stamp {
            inner.map.clear();
            inner.stamp = post.clone();
        }
        if !results.hits.is_empty() {
            inner.map.put(
                key.clone(),
                Entry {
                    results: results.clone(),
                    expires_at: now().checked_add(self.ttl).unwrap_or_else(now),
                },
            );
        }
        metrics::gauge!(metric_names::SEARCH_CACHE_ENTRIES).set(inner.map.len() as f64);
    }
}

fn now() -> tokio::time::Instant {
    tokio::time::Instant::now()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use dc3_search::Hit;

    use super::*;

    fn handle() -> (tempfile::TempDir, SearchHandle) {
        let dir = tempfile::tempdir().unwrap();
        let search = SearchHandle::open(dir.path()).unwrap();
        (dir, search)
    }

    fn config(entries: usize, ttl: Duration) -> SearchCacheConfig {
        SearchCacheConfig::new(entries, ttl)
    }

    fn key(text: &str) -> CacheKey {
        CacheKey {
            text: text.to_owned(),
            sort: Sort::Relevance,
            page: 1,
            per_page: 20,
        }
    }

    fn results(ids: &[i64]) -> SearchResults {
        SearchResults {
            hits: ids.iter().map(|&id| Hit { id, score: 1.0 }).collect(),
            total: ids.len() as u64,
        }
    }

    /// Runs the leader path: `pre_search` must miss, then installs `flight`.
    fn miss_and_install(
        cache: &SearchCache,
        search: &SearchHandle,
        key: &CacheKey,
        flight: Flight,
    ) {
        let cell = match cache.pre_search(key, search).unwrap() {
            PreSearch::Lead(cell) => cell,
            PreSearch::Hit(_) | PreSearch::Wait(_) => panic!("expected a leader miss"),
        };
        let flight = std::sync::Arc::new(flight);
        cell.set(std::sync::Arc::clone(&flight)).unwrap();
        cache.install(key, &cell, flight);
    }

    #[test]
    fn zero_size_or_ttl_disables() {
        let (_dir, search) = handle();
        let stamp = search.stamp();
        for cfg in [
            config(0, Duration::from_secs(60)),
            config(100, Duration::ZERO),
            config(0, Duration::ZERO),
        ] {
            let cache = SearchCache::new(cfg, stamp.clone());
            assert!(cache.is_disabled());
            assert_eq!(cache.len(), 0);
            // Disabled skips everything: no stamp read, straight to `None`.
            assert!(cache.pre_search(&key("q"), &search).is_none());
        }
        assert!(SearchCache::disabled().is_disabled());
    }

    #[test]
    fn hit_and_key_components() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        let k = key("hello");
        miss_and_install(&cache, &search, &k, Ok((results(&[1, 2]), stamp)));
        assert_eq!(cache.len(), 1);
        let cached = match cache.pre_search(&k, &search).unwrap() {
            PreSearch::Hit(r) => r,
            _ => panic!("expected a hit"),
        };
        assert_eq!(cached.hits.iter().map(|h| h.id).collect::<Vec<_>>(), [1, 2]);

        // Every key component misses on its own.
        let mut other = k.clone();
        other.text = "other".into();
        let mut sort = k.clone();
        sort.sort = Sort::Newest;
        let mut page = k.clone();
        page.page = 2;
        let mut per_page = k.clone();
        per_page.per_page = 10;
        for (i, variant) in [other, sort, page, per_page].into_iter().enumerate() {
            assert!(
                matches!(
                    cache.pre_search(&variant, &search).unwrap(),
                    PreSearch::Lead(_)
                ),
                "key component {i} should miss"
            );
        }
    }

    #[test]
    fn lru_evicts_oldest() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(2, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        let (a, b, c) = (key("a"), key("b"), key("c"));
        miss_and_install(&cache, &search, &a, Ok((results(&[1]), stamp.clone())));
        miss_and_install(&cache, &search, &b, Ok((results(&[2]), stamp.clone())));
        // Touch `a` so `b` is the eviction victim.
        assert!(matches!(
            cache.pre_search(&a, &search).unwrap(),
            PreSearch::Hit(_)
        ));
        miss_and_install(&cache, &search, &c, Ok((results(&[3]), stamp)));
        assert_eq!(cache.len(), 2);
        assert!(matches!(
            cache.pre_search(&a, &search).unwrap(),
            PreSearch::Hit(_)
        ));
        assert!(matches!(
            cache.pre_search(&c, &search).unwrap(),
            PreSearch::Hit(_)
        ));
        // `b` was evicted by the N+1 insert.
        assert!(matches!(
            cache.pre_search(&b, &search).unwrap(),
            PreSearch::Lead(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn ttl_is_fixed_from_insert() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        let k = key("q");
        miss_and_install(&cache, &search, &k, Ok((results(&[1]), stamp)));

        // Halfway through: a hit refreshes recency, never expiry.
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(matches!(
            cache.pre_search(&k, &search).unwrap(),
            PreSearch::Hit(_)
        ));
        // Past the original insert: expired despite the mid-life hit.
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(matches!(
            cache.pre_search(&k, &search).unwrap(),
            PreSearch::Lead(_)
        ));
        // `len()` still counts the physical entry until the `get` above;
        // the expired entry was popped by that lookup.
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_entries_are_replaced() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(10)), search.stamp());
        let stamp = search.stamp();
        let k = key("q");
        miss_and_install(&cache, &search, &k, Ok((results(&[1]), stamp.clone())));
        tokio::time::advance(Duration::from_secs(11)).await;
        let cell = match cache.pre_search(&k, &search).unwrap() {
            PreSearch::Lead(cell) => cell,
            _ => panic!("expected expiry"),
        };
        let flight = std::sync::Arc::new(Ok((results(&[2]), stamp)));
        cell.set(std::sync::Arc::clone(&flight)).unwrap();
        cache.install(&k, &cell, flight);
        assert_eq!(cache.len(), 1);
        let cached = match cache.pre_search(&k, &search).unwrap() {
            PreSearch::Hit(r) => r,
            _ => panic!("expected a hit after replacement"),
        };
        assert_eq!(cached.hits.iter().map(|h| h.id).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn empty_successes_are_never_inserted() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        let k = key("gibberish");
        miss_and_install(&cache, &search, &k, Ok((results(&[]), stamp.clone())));
        assert_eq!(cache.len(), 0);
        // Still a miss next time: empties are shared by a flight only.
        assert!(matches!(
            cache.pre_search(&k, &search).unwrap(),
            PreSearch::Lead(_)
        ));
    }

    #[test]
    fn errors_are_never_inserted() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let k = key("q");
        miss_and_install(&cache, &search, &k, Err(Failure::Busy));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn stamp_mismatch_clears_everything() {
        let (dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        let (a, b) = (key("a"), key("b"));
        miss_and_install(&cache, &search, &a, Ok((results(&[1]), stamp.clone())));
        miss_and_install(&cache, &search, &b, Ok((results(&[2]), stamp)));
        assert_eq!(cache.len(), 2);

        // A commit changes the stamp once reloaded.
        let index = search.index();
        let mut writer = index.writer(20 * 1024 * 1024).unwrap();
        writer
            .upsert(&dc3_search::IndexDoc {
                id: 1,
                name: "a b".into(),
                files: String::new(),
                size: 1,
                created: 1,
                seen: 0,
                file_count: 1,
            })
            .unwrap();
        writer.commit(1).unwrap();
        index.reload().unwrap();
        assert!(!search.stamp_matches(&cache.lock().unwrap().stamp));

        // Next request clears the whole map and adopts the new stamp.
        assert!(matches!(
            cache.pre_search(&a, &search).unwrap(),
            PreSearch::Lead(_)
        ));
        assert_eq!(cache.len(), 0);
        assert!(search.stamp_matches(&cache.lock().unwrap().stamp));
        // No per-entry stale residue: `b` misses too.
        assert!(matches!(
            cache.pre_search(&b, &search).unwrap(),
            PreSearch::Lead(_)
        ));
        drop(dir);
    }

    #[tokio::test]
    async fn identical_misses_share_one_flight() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let k = key("q");
        let lead = match cache.pre_search(&k, &search).unwrap() {
            PreSearch::Lead(cell) => cell,
            _ => panic!("first miss leads"),
        };
        let wait = match cache.pre_search(&k, &search).unwrap() {
            PreSearch::Wait(cell) => cell,
            _ => panic!("second miss waits"),
        };
        assert!(std::sync::Arc::ptr_eq(&lead, &wait));

        // The leader resolves; the waiter shares the outcome with no
        // second search.
        let flight: std::sync::Arc<Flight> =
            std::sync::Arc::new(Ok((results(&[7]), search.stamp())));
        lead.set(std::sync::Arc::clone(&flight)).unwrap();
        let shared = wait.get().unwrap();
        assert_eq!(shared.as_ref().as_ref().unwrap().0.hits[0].id, 7);
        cache.install(&k, &lead, flight);
        assert_eq!(cache.len(), 1);
        // The flight entry is removed on install.
        assert!(cache.lock().unwrap().inflight.is_empty());
    }

    #[test]
    fn clear_empties_the_map() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        miss_and_install(&cache, &search, &key("q"), Ok((results(&[1]), stamp)));
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    /// Joins the open flight for `cell`, running the init future if this
    /// caller gets there first. Returns the shared flight.
    async fn join_flight(
        cell: &std::sync::Arc<tokio::sync::OnceCell<std::sync::Arc<Flight>>>,
        init_count: &std::sync::atomic::AtomicUsize,
        outcome: Flight,
    ) -> std::sync::Arc<Flight> {
        let flight = cell
            .get_or_init(|| {
                init_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                async move { std::sync::Arc::new(outcome) }
            })
            .await;
        std::sync::Arc::clone(flight)
    }

    #[allow(clippy::arithmetic_side_effects)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_distinct_keys_share_nothing_and_block_nothing() {
        const KEYS: usize = 8;
        const PER_KEY: usize = 8;
        let (_dir, search) = handle();
        let search = std::sync::Arc::new(search);
        let cache = std::sync::Arc::new(SearchCache::new(
            config(64, Duration::from_secs(60)),
            search.stamp(),
        ));
        let stamp = search.stamp();
        let keys: Vec<CacheKey> = (0..KEYS).map(|i| key(&format!("hammer-{i}"))).collect();
        let want: Vec<i64> = vec![101, 102, 103, 104, 105, 106, 107, 108];
        let inits: Vec<std::sync::Arc<std::sync::atomic::AtomicUsize>> = (0..KEYS)
            .map(|_| std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .collect();
        let start = std::sync::Arc::new(tokio::sync::Barrier::new(KEYS * PER_KEY));
        let mut tasks = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            for _ in 0..PER_KEY {
                let (cache, search, start, init, outcome_stamp) = (
                    std::sync::Arc::clone(&cache),
                    std::sync::Arc::clone(&search),
                    std::sync::Arc::clone(&start),
                    std::sync::Arc::clone(&inits[i]),
                    stamp.clone(),
                );
                let (k, want_id) = (k.clone(), want[i]);
                tasks.push(tokio::spawn(async move {
                    start.wait().await;
                    // Each task builds its own flight value: `Flight` is
                    // deliberately not `Clone` (`Failure` isn't), sharing
                    // happens through the `Arc` in the cell.
                    let outcome: Flight = Ok((results(&[want_id]), outcome_stamp));
                    match cache.pre_search(&k, &search) {
                        Some(PreSearch::Hit(r)) => {
                            assert_eq!(r.hits.len(), 1);
                            assert_eq!(r.hits[0].id, want_id);
                            (i, false)
                        }
                        Some(PreSearch::Lead(cell)) => {
                            let flight = join_flight(&cell, &init, outcome).await;
                            let (r, _) = flight.as_ref().as_ref().unwrap();
                            assert_eq!(r.hits.len(), 1);
                            assert_eq!(r.hits[0].id, want_id);
                            cache.install(&k, &cell, flight);
                            (i, true)
                        }
                        Some(PreSearch::Wait(cell)) => {
                            let flight = join_flight(&cell, &init, outcome).await;
                            let (r, _) = flight.as_ref().as_ref().unwrap();
                            assert_eq!(r.hits.len(), 1);
                            assert_eq!(r.hits[0].id, want_id);
                            (i, true)
                        }
                        None => panic!("cache is enabled"),
                    }
                }));
            }
        }
        // Every task finishes: no panic, no deadlock, no cross-key block.
        let mut hits = [0usize; KEYS];
        let mut shared = [0usize; KEYS];
        for task in tasks {
            let (i, was_shared) = task.await.unwrap();
            if was_shared {
                shared[i] += 1;
            } else {
                hits[i] += 1;
            }
        }
        for i in 0..KEYS {
            // Exactly one flight ran per key under every interleaving: the
            // first miss leads, later misses wait or hit, and installing a
            // non-empty success always leaves a live entry behind.
            assert_eq!(inits[i].load(std::sync::atomic::Ordering::Relaxed), 1);
            assert_eq!(hits[i] + shared[i], PER_KEY);
            assert!(shared[i] >= 1, "the first miss always shares");
        }
        assert_eq!(cache.len(), KEYS);
        assert!(cache.lock().unwrap().inflight.is_empty());
        // Every key serves hits from here on.
        for k in &keys {
            assert!(matches!(
                cache.pre_search(k, &search).unwrap(),
                PreSearch::Hit(_)
            ));
        }
    }

    #[tokio::test]
    async fn unresolved_flight_on_one_key_blocks_no_other_key() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(16, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        let (ka, kb) = (key("a"), key("b"));
        // Key A's flight stays open: the leader resolves it only at the end.
        let lead_a = match cache.pre_search(&ka, &search).unwrap() {
            PreSearch::Lead(cell) => cell,
            _ => panic!("first miss leads"),
        };
        // Key B runs a full miss-install-hit cycle while A's flight is open.
        // The timeout turns any cross-key blocking into a loud failure.
        tokio::time::timeout(Duration::from_secs(10), async {
            miss_and_install(&cache, &search, &kb, Ok((results(&[2]), stamp.clone())));
            assert!(matches!(
                cache.pre_search(&kb, &search).unwrap(),
                PreSearch::Hit(_)
            ));
        })
        .await
        .expect("key B must complete while key A's flight is open");
        assert_eq!(cache.len(), 1);
        // A resolves late; both entries coexist.
        let flight_a = std::sync::Arc::new(Ok((results(&[1]), stamp)));
        lead_a.set(std::sync::Arc::clone(&flight_a)).unwrap();
        cache.install(&ka, &lead_a, flight_a);
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_errors_share_one_flight_and_insert_nothing() {
        let (_dir, search) = handle();
        let search = std::sync::Arc::new(search);
        let cache = std::sync::Arc::new(SearchCache::new(
            config(16, Duration::from_secs(60)),
            search.stamp(),
        ));
        let k = key("q");
        // The leader misses first, so every waiter below deterministically
        // finds the open flight: none can arrive post-install.
        let lead = match cache.pre_search(&k, &search).unwrap() {
            PreSearch::Lead(cell) => cell,
            _ => panic!("first miss leads"),
        };
        let flight: std::sync::Arc<Flight> = std::sync::Arc::new(Err(Failure::Busy));
        let go = std::sync::Arc::new(tokio::sync::Notify::new());
        let arrived = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..15 {
            let (cache, search, go, arrived, flight) = (
                std::sync::Arc::clone(&cache),
                std::sync::Arc::clone(&search),
                std::sync::Arc::clone(&go),
                std::sync::Arc::clone(&arrived),
                std::sync::Arc::clone(&flight),
            );
            let k = k.clone();
            tasks.push(tokio::spawn(async move {
                let waiter = match cache.pre_search(&k, &search).unwrap() {
                    PreSearch::Wait(cell) => cell,
                    _ => panic!("concurrent miss waits"),
                };
                // Register before signalling: the leader notifies only after
                // all 15 arrivals are counted, so no wakeup can be missed.
                let wake = go.notified();
                arrived.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                wake.await;
                let shared = waiter
                    .get_or_init(|| async { std::sync::Arc::clone(&flight) })
                    .await;
                assert!(shared.as_ref().is_err());
            }));
        }
        // Once all waiters park on the open flight, run the single search,
        // install it (errors drop the flight but insert nothing), and wake
        // everyone to share the outcome. A finished task before full arrival
        // means a waiter panicked: fail loudly instead of spinning forever.
        while arrived.load(std::sync::atomic::Ordering::Relaxed) != 15 {
            if tasks.iter().any(|t| t.is_finished()) {
                panic!("a waiter finished before all arrived");
            }
            tokio::task::yield_now().await;
        }
        let run = lead
            .get_or_init(|| {
                inits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let flight = std::sync::Arc::clone(&flight);
                async move { flight }
            })
            .await;
        assert!(run.as_ref().is_err());
        cache.install(&k, &lead, std::sync::Arc::clone(run));
        go.notify_waiters();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(inits.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(cache.len(), 0);
        assert!(cache.lock().unwrap().inflight.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_empties_share_one_flight_and_insert_nothing() {
        let (_dir, search) = handle();
        let search = std::sync::Arc::new(search);
        let cache = std::sync::Arc::new(SearchCache::new(
            config(16, Duration::from_secs(60)),
            search.stamp(),
        ));
        let k = key("q");
        // Same rendezvous as the error case: the leader misses first, so all
        // 15 waiters deterministically share its flight.
        let lead = match cache.pre_search(&k, &search).unwrap() {
            PreSearch::Lead(cell) => cell,
            _ => panic!("first miss leads"),
        };
        let flight: std::sync::Arc<Flight> =
            std::sync::Arc::new(Ok((results(&[]), search.stamp())));
        let go = std::sync::Arc::new(tokio::sync::Notify::new());
        let arrived = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..15 {
            let (cache, search, go, arrived, flight) = (
                std::sync::Arc::clone(&cache),
                std::sync::Arc::clone(&search),
                std::sync::Arc::clone(&go),
                std::sync::Arc::clone(&arrived),
                std::sync::Arc::clone(&flight),
            );
            let k = k.clone();
            tasks.push(tokio::spawn(async move {
                let waiter = match cache.pre_search(&k, &search).unwrap() {
                    PreSearch::Wait(cell) => cell,
                    _ => panic!("concurrent miss waits"),
                };
                let wake = go.notified();
                arrived.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                wake.await;
                let shared = waiter
                    .get_or_init(|| async { std::sync::Arc::clone(&flight) })
                    .await;
                let (r, _) = shared.as_ref().as_ref().unwrap();
                assert!(r.hits.is_empty());
            }));
        }
        while arrived.load(std::sync::atomic::Ordering::Relaxed) != 15 {
            if tasks.iter().any(|t| t.is_finished()) {
                panic!("a waiter finished before all arrived");
            }
            tokio::task::yield_now().await;
        }
        let run = lead
            .get_or_init(|| {
                inits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let flight = std::sync::Arc::clone(&flight);
                async move { flight }
            })
            .await;
        let (r, _) = run.as_ref().as_ref().unwrap();
        assert!(r.hits.is_empty());
        cache.install(&k, &lead, std::sync::Arc::clone(run));
        go.notify_waiters();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(inits.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(cache.len(), 0);
        assert!(cache.lock().unwrap().inflight.is_empty());
    }

    #[test]
    fn poisoned_lock_recovers() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        // Poison the inner mutex by panicking while holding the guard.
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = cache.inner.as_ref().unwrap().lock().unwrap();
            panic!("intentional poison");
        }));
        assert!(poisoned.is_err());
        // `lock()` recovers via `PoisonError::into_inner`: the cache serves on.
        let stamp = search.stamp();
        let k = key("q");
        miss_and_install(&cache, &search, &k, Ok((results(&[1]), stamp)));
        assert_eq!(cache.len(), 1);
        assert!(matches!(
            cache.pre_search(&k, &search).unwrap(),
            PreSearch::Hit(_)
        ));
    }

    #[test]
    fn unique_query_flood_inserts_only_non_empty_successes() {
        let (_dir, search) = handle();
        let cache = SearchCache::new(config(100, Duration::from_secs(60)), search.stamp());
        let stamp = search.stamp();
        // 150 distinct non-empty successes through a 100-entry cache: every
        // query is served and the map stays bounded by LRU capacity.
        for i in 0..150 {
            let k = key(&format!("flood-{i}"));
            miss_and_install(&cache, &search, &k, Ok((results(&[1]), stamp.clone())));
        }
        assert_eq!(cache.len(), 100);
        // Floods of errors and empties insert nothing and fail nothing:
        // availability degrades to the uncached behavior, never to errors.
        for i in 0..50 {
            let k = key(&format!("flood-err-{i}"));
            miss_and_install(&cache, &search, &k, Err(Failure::Busy));
            let k = key(&format!("flood-empty-{i}"));
            miss_and_install(&cache, &search, &k, Ok((results(&[]), stamp.clone())));
        }
        assert_eq!(cache.len(), 100);
    }
}
