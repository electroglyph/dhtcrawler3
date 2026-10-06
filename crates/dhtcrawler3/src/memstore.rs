//! An in-memory [`CrawlStore`] for tests and experiments.
//!
//! It follows the queue rules of `dc3-store` closely enough for the pipeline
//! tests: observations queue new keys, claims lease them, `complete` stores a
//! torrent unless a key is denied, `fail` backs off and gives up, and `deny`
//! tombstones. Nothing is persisted.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dc3_core::DhtKey;
use dc3_store::{
    DenyOutcome, DenyReason, MAX_FETCH_ATTEMPTS, NewTorrent, Observation, ObserveOutcome,
    PendingItem, Result, ScrapeItem, StoreError,
};
use tokio::time::Instant;

use crate::stores::CrawlStore;

/// Length of the key prefix denials are matched on.
const DENY_PREFIX_LEN: usize = 20;
/// Retry delay after the first failure; doubled per failure.
pub const MEMORY_FAIL_BACKOFF: Duration = Duration::from_secs(300);

/// One recorded denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    pub reason: DenyReason,
    pub note: Option<String>,
    pub actor: String,
}

#[derive(Debug, Clone)]
struct Pending {
    seen: u64,
    attempts: u32,
    next_attempt: Instant,
    lease_until: Option<Instant>,
    gave_up: bool,
    /// Last piggybacked seeder estimate (`None` means unscraped).
    seeders: Option<u32>,
}

#[derive(Debug, Default)]
struct State {
    pending: BTreeMap<DhtKey, Pending>,
    torrents: HashMap<DhtKey, (i64, NewTorrent)>,
    denied: HashMap<[u8; DENY_PREFIX_LEN], Denial>,
    next_id: i64,
    observe_calls: usize,
    observed: Vec<Observation>,
    failing_observes: usize,
    renewals: usize,
    fails: HashMap<DhtKey, u32>,
    /// `None` means [`MEMORY_FAIL_BACKOFF`].
    fail_backoff: Option<Duration>,
    scrapes: HashMap<DhtKey, MemScrape>,
    tombstoned: HashMap<DhtKey, (Instant, i64, u64)>,
    removed: HashMap<[u8; DENY_PREFIX_LEN], RemovedEntry>,
}

/// Scrape bookkeeping of one stored torrent. `version` plays the role of
/// both `last_seen_at` and `change_seq` in [`CrawlStore::tombstone_dead`]:
/// every refresh bumps it, so a stale snapshot never matches.
#[derive(Debug, Clone)]
struct MemScrape {
    id: i64,
    est: Option<u32>,
    failures: u32,
    last_scraped: Option<Instant>,
    seen_at: DateTime<Utc>,
    version: u64,
}

/// Removal memory of one DHT key (see `removed_keys` in `dc3-store`).
#[derive(Debug, Clone)]
struct RemovedEntry {
    removed_at: Instant,
    removals: u32,
    sightings: u32,
}

/// An in-memory crawl store. Clones share the same data.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore {
    state: Arc<Mutex<State>>,
}

fn prefix(key: &[u8]) -> [u8; DENY_PREFIX_LEN] {
    let mut out = [0u8; DENY_PREFIX_LEN];
    for (dst, src) in out.iter_mut().zip(key) {
        *dst = *src;
    }
    out
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store whose first retry after a failure waits `backoff`
    /// (doubled per failure) instead of [`MEMORY_FAIL_BACKOFF`].
    pub fn with_fail_backoff(backoff: Duration) -> Self {
        let store = Self::default();
        store.lock().fail_backoff = Some(backoff);
        store
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The stored torrent for `key`, if any.
    pub fn torrent(&self, key: &DhtKey) -> Option<NewTorrent> {
        self.lock().torrents.get(key).map(|(_, t)| t.clone())
    }

    /// Number of stored torrents.
    pub fn torrent_count(&self) -> usize {
        self.lock().torrents.len()
    }

    /// The denial covering `key` (matched on its first 20 bytes).
    pub fn denial(&self, key: &[u8]) -> Option<Denial> {
        self.lock().denied.get(&prefix(key)).cloned()
    }

    /// Denies `key` without any other effect, as an admin would beforehand.
    pub fn preload_denial(&self, key: &[u8], reason: DenyReason) {
        self.lock().denied.insert(
            prefix(key),
            Denial {
                reason,
                note: None,
                actor: "test".into(),
            },
        );
    }

    /// Keys in the queue that have not given up.
    pub fn pending_keys(&self) -> Vec<DhtKey> {
        self.lock()
            .pending
            .iter()
            .filter(|(_, p)| !p.gave_up)
            .map(|(k, _)| *k)
            .collect()
    }

    /// Queues `key` directly.
    pub fn enqueue(&self, key: DhtKey) {
        self.lock().pending.entry(key).or_insert(Pending {
            seen: 1,
            attempts: 0,
            next_attempt: Instant::now(),
            lease_until: None,
            gave_up: false,
            seeders: None,
        });
    }

    /// Number of `observe` calls, including failed ones.
    pub fn observe_calls(&self) -> usize {
        self.lock().observe_calls
    }

    /// Every observation of every successful `observe` call, in order.
    pub fn observed(&self) -> Vec<Observation> {
        self.lock().observed.clone()
    }

    /// Makes the next `n` `observe` calls fail.
    pub fn fail_next_observes(&self, n: usize) {
        self.lock().failing_observes = n;
    }

    /// Number of successful lease renewals.
    pub fn renewals(&self) -> usize {
        self.lock().renewals
    }

    /// Number of `fail` and `give_up` calls for `key`.
    pub fn failures(&self, key: &DhtKey) -> u32 {
        self.lock().fails.get(key).copied().unwrap_or(0)
    }

    /// The piggybacked seeder estimate queued for `key`, if any.
    pub fn pending_seeders(&self, key: &DhtKey) -> Option<u32> {
        self.lock().pending.get(key).and_then(|p| p.seeders)
    }

    fn is_denied(state: &State, keys: &[&[u8]]) -> bool {
        keys.iter().any(|k| state.denied.contains_key(&prefix(k)))
    }
}

impl CrawlStore for MemoryStore {
    async fn observe(&self, batch: &[Observation], max_pending: i64) -> Result<ObserveOutcome> {
        let mut state = self.lock();
        state.observe_calls = state.observe_calls.saturating_add(1);
        if state.failing_observes > 0 {
            state.failing_observes = state.failing_observes.saturating_sub(1);
            return Err(StoreError::Invalid("injected observe failure".into()));
        }
        let mut merged: BTreeMap<DhtKey, (u64, bool)> = BTreeMap::new();
        for o in batch.iter().filter(|o| o.sightings > 0) {
            let entry = merged.entry(o.key).or_insert((0, false));
            entry.0 = entry.0.saturating_add(u64::from(o.sightings));
            entry.1 |= o.priority;
        }
        let full = i64::try_from(state.pending.len()).unwrap_or(i64::MAX) >= max_pending;
        let mut out = ObserveOutcome::default();
        let now = Instant::now();
        for (key, (n, priority)) in merged {
            if state.torrents.contains_key(&key) {
                out.known = out.known.saturating_add(1);
                // A sighting refreshes the row: bump the scrape guard so a
                // stale tombstone snapshot cannot match afterwards.
                if let Some(sc) = state.scrapes.get_mut(&key) {
                    sc.version = sc.version.saturating_add(1);
                    sc.seen_at = Utc::now();
                }
            } else if Self::is_denied(&state, &[key.as_bytes()]) {
                out.denied = out.denied.saturating_add(1);
            } else if let Some(p) = state.pending.get_mut(&key) {
                p.seen = p.seen.saturating_add(n);
            } else if full && !priority {
                out.dropped = out.dropped.saturating_add(1);
            } else {
                state.pending.insert(
                    key,
                    Pending {
                        seen: n,
                        attempts: 0,
                        next_attempt: now,
                        lease_until: None,
                        gave_up: false,
                        seeders: None,
                    },
                );
                out.queued = out.queued.saturating_add(1);
            }
        }
        state.observed.extend_from_slice(batch);
        Ok(out)
    }

    async fn claim(&self, n: i64, lease: Duration) -> Result<Vec<PendingItem>> {
        let mut state = self.lock();
        let now = Instant::now();
        let limit = usize::try_from(n.max(0)).unwrap_or(0);
        // Liveness first (known-live keys), then oldest attempt: mirrors
        // CLAIM_SQL's ORDER BY seeders_est DESC NULLS LAST, next_attempt_at.
        let mut due: Vec<(Option<u32>, Instant, DhtKey)> = state
            .pending
            .iter()
            .filter(|(_, p)| {
                !p.gave_up && p.next_attempt <= now && p.lease_until.is_none_or(|l| l < now)
            })
            .map(|(k, p)| (p.seeders, p.next_attempt, *k))
            .collect();
        due.sort_by(|a, b| {
            match (a.0, b.0) {
                (Some(x), Some(y)) => y.cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
        });
        let mut out = Vec::new();
        for (_, _, key) in due.into_iter().take(limit) {
            if let Some(p) = state.pending.get_mut(&key) {
                p.lease_until = Some(now + lease);
                out.push(PendingItem {
                    dht_key: key,
                    attempts: p.attempts,
                    seen_count: p.seen,
                });
            }
        }
        Ok(out)
    }

    async fn renew(&self, key: &DhtKey, lease: Duration) -> Result<bool> {
        let mut state = self.lock();
        let now = Instant::now();
        let renewed = match state.pending.get_mut(key) {
            Some(p) if !p.gave_up && p.lease_until.is_some_and(|l| l >= now) => {
                let until = now + lease;
                if p.lease_until.is_none_or(|l| l < until) {
                    p.lease_until = Some(until);
                }
                true
            }
            _ => false,
        };
        if renewed {
            state.renewals = state.renewals.saturating_add(1);
        }
        Ok(renewed)
    }

    async fn complete(&self, key: &DhtKey, t: &NewTorrent) -> Result<i64> {
        if t.dht_key != *key {
            return Err(StoreError::Invalid(
                "NewTorrent.dht_key differs from the completed key".into(),
            ));
        }
        // Like the database store: sizes must fit its bigint columns.
        let sizes = std::iter::once(t.total_size).chain(t.files.iter().map(|f| f.size));
        if sizes.into_iter().any(|v| i64::try_from(v).is_err()) {
            return Err(StoreError::Invalid("size exceeds i64".into()));
        }
        let mut state = self.lock();
        state.pending.remove(key);
        let v1 = t.info_hash_v1.map(|k| k.0);
        let v2 = t.info_hash_v2.map(|h| h.0);
        let mut keys: Vec<&[u8]> = vec![key.as_bytes()];
        if let Some(k) = &v1 {
            keys.push(k);
        }
        if let Some(h) = &v2 {
            keys.push(h);
        }
        if Self::is_denied(&state, &keys) {
            return Err(StoreError::Denied);
        }
        let id = match state.torrents.get(key) {
            Some((id, _)) => *id,
            None => {
                state.next_id = state.next_id.saturating_add(1);
                state.next_id
            }
        };
        state.torrents.insert(*key, (id, t.clone()));
        // A successful fetch refreshes the row and clears removal memory
        // (positive liveness proof), and revives a tombstone.
        let entry = state.scrapes.entry(*key).or_insert(MemScrape {
            id,
            est: None,
            failures: 0,
            last_scraped: None,
            seen_at: Utc::now(),
            version: 0,
        });
        entry.id = id;
        entry.version = entry.version.saturating_add(1);
        entry.seen_at = Utc::now();
        state.tombstoned.remove(key);
        state.removed.remove(&prefix(key.as_bytes()));
        Ok(id)
    }

    async fn fail(&self, key: &DhtKey) -> Result<bool> {
        let mut state = self.lock();
        let count = state.fails.entry(*key).or_insert(0);
        *count = count.saturating_add(1);
        let now = Instant::now();
        let base = state.fail_backoff.unwrap_or(MEMORY_FAIL_BACKOFF);
        let Some(p) = state.pending.get_mut(key) else {
            return Ok(false);
        };
        let doubling = 2u32.saturating_pow(p.attempts);
        p.attempts = p.attempts.saturating_add(1);
        p.next_attempt = now
            .checked_add(base.saturating_mul(doubling))
            .unwrap_or(now);
        p.lease_until = None;
        let max = u32::try_from(MAX_FETCH_ATTEMPTS).unwrap_or(u32::MAX);
        p.gave_up = p.attempts >= max;
        Ok(p.gave_up)
    }

    async fn give_up(&self, key: &DhtKey) -> Result<bool> {
        let mut state = self.lock();
        let count = state.fails.entry(*key).or_insert(0);
        *count = count.saturating_add(1);
        let Some(p) = state.pending.get_mut(key) else {
            return Ok(false);
        };
        p.attempts = p.attempts.saturating_add(1);
        p.lease_until = None;
        p.gave_up = true;
        Ok(true)
    }

    async fn deny(
        &self,
        key: &[u8],
        reason: DenyReason,
        note: Option<&str>,
        actor: &str,
    ) -> Result<DenyOutcome> {
        if key.len() != DhtKey::LEN && key.len() != dc3_core::InfoHashV2::LEN {
            return Err(StoreError::Invalid("key must be 20 or 32 bytes".into()));
        }
        let mut state = self.lock();
        let p = prefix(key);
        let newly_denied = !state.denied.contains_key(&p);
        if newly_denied {
            state.denied.insert(
                p,
                Denial {
                    reason,
                    note: note.map(str::to_owned),
                    actor: actor.to_owned(),
                },
            );
        }
        let before = state.torrents.len();
        state.torrents.retain(|k, (_, t)| {
            let v1 = t.info_hash_v1.map(|k| k.0);
            let v2 = t.info_hash_v2.map(|h| prefix(&h.0));
            k.0 != p && v1 != Some(p) && v2 != Some(p)
        });
        let tombstoned = before.saturating_sub(state.torrents.len());
        state.pending.remove(&DhtKey(p));
        Ok(DenyOutcome {
            newly_denied,
            tombstoned: u64::try_from(tombstoned).unwrap_or(u64::MAX),
        })
    }

    async fn pending_depth(&self) -> Result<i64> {
        Ok(i64::try_from(self.lock().pending.len()).unwrap_or(i64::MAX))
    }

    async fn claim_scrape_due(
        &self,
        limit: i64,
        live_interval: Duration,
        unknown_interval: Duration,
    ) -> Result<Vec<ScrapeItem>> {
        let mut state = self.lock();
        let now = Instant::now();
        let mut due: Vec<(Option<Instant>, DhtKey)> = Vec::new();
        for key in state.torrents.keys() {
            if Self::is_denied(&state, &[key.as_bytes()]) {
                continue;
            }
            let last = state.scrapes.get(key).and_then(|sc| sc.last_scraped);
            let est = state.scrapes.get(key).and_then(|sc| sc.est);
            let stale = match (last, est) {
                (None, _) => true,
                (Some(at), None) => now.saturating_duration_since(at) >= unknown_interval,
                (Some(at), Some(_)) => now.saturating_duration_since(at) >= live_interval,
            };
            if stale {
                due.push((last, *key));
            }
        }
        due.sort();
        let cap = usize::try_from(limit.max(0)).unwrap_or(0);
        let mut out = Vec::new();
        for (_, key) in due.into_iter().take(cap) {
            let id = state.torrents.get(&key).map_or(0, |(id, _)| *id);
            let sc = state.scrapes.entry(key).or_insert(MemScrape {
                id,
                est: None,
                failures: 0,
                last_scraped: None,
                seen_at: Utc::now(),
                version: 0,
            });
            sc.last_scraped = Some(now);
            out.push(ScrapeItem {
                id: sc.id,
                dht_key: key,
                seeders_est: sc.est,
                scrape_failures: sc.failures,
                last_seen_at: sc.seen_at,
                change_seq: i64::try_from(sc.version).unwrap_or(i64::MAX),
            });
        }
        Ok(out)
    }

    async fn record_scrape(
        &self,
        id: i64,
        seeders_est: Option<u32>,
        scrape_failures: u32,
    ) -> Result<bool> {
        let mut state = self.lock();
        match state.scrapes.values_mut().find(|sc| sc.id == id) {
            Some(sc) => {
                sc.est = seeders_est;
                sc.failures = scrape_failures;
                sc.last_scraped = Some(Instant::now());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn tombstone_dead(
        &self,
        id: i64,
        old_last_seen_at: DateTime<Utc>,
        old_change_seq: i64,
    ) -> Result<bool> {
        let mut state = self.lock();
        let key = state
            .torrents
            .iter()
            .find(|(_, (tid, _))| *tid == id)
            .map(|(k, _)| *k);
        let Some(key) = key else {
            return Ok(false);
        };
        let guarded = state.scrapes.get(&key).is_some_and(|sc| {
            sc.seen_at == old_last_seen_at
                && sc.version == u64::try_from(old_change_seq).unwrap_or(u64::MAX)
        });
        if !guarded {
            return Ok(false);
        }
        let size = state.torrents.get(&key).map_or(0, |(_, t)| t.total_size);
        state.torrents.remove(&key);
        state.scrapes.remove(&key);
        state.tombstoned.insert(key, (Instant::now(), id, size));
        let entry = state.removed.entry(prefix(key.as_bytes())).or_insert(RemovedEntry {
            removed_at: Instant::now(),
            removals: 0,
            sightings: 0,
        });
        entry.removed_at = Instant::now();
        entry.removals = entry.removals.saturating_add(1);
        entry.sightings = 0;
        Ok(true)
    }

    async fn purge_tombstoned(&self, grace: Duration, limit: i64) -> Result<u64> {
        let mut state = self.lock();
        let now = Instant::now();
        // Biggest first under the cap (win 5): each sweep frees the most disk.
        let mut doomed: Vec<(u64, DhtKey)> = state
            .tombstoned
            .iter()
            .filter(|(key, (at, _, _))| {
                now.saturating_duration_since(*at) >= grace
                    && !Self::is_denied(&state, &[key.as_bytes()])
            })
            .map(|(key, (_, _, size))| (*size, *key))
            .collect();
        doomed.sort();
        doomed.reverse();
        let cap = usize::try_from(limit.max(0)).unwrap_or(0);
        let n = doomed.len().min(cap);
        for (_, key) in doomed.into_iter().take(n) {
            state.tombstoned.remove(&key);
        }
        Ok(u64::try_from(n).unwrap_or(u64::MAX))
    }

    async fn note_fetch_estimate(&self, key: &DhtKey, seeders_est: u32) -> Result<()> {
        let mut state = self.lock();
        if let Some(p) = state.pending.get_mut(key) {
            p.seeders = Some(seeders_est);
        }
        Ok(())
    }

    async fn trim_removed_keys(&self, cap: i64) -> Result<u64> {
        let mut state = self.lock();
        let len = state.removed.len();
        let excess = len.saturating_sub(usize::try_from(cap.max(0)).unwrap_or(0));
        if excess == 0 {
            return Ok(0);
        }
        let mut oldest: Vec<(Instant, [u8; DENY_PREFIX_LEN])> = state
            .removed
            .iter()
            .map(|(k, e)| (e.removed_at, *k))
            .collect();
        oldest.sort();
        for (_, key) in oldest.into_iter().take(excess) {
            state.removed.remove(&key);
        }
        Ok(u64::try_from(excess).unwrap_or(u64::MAX))
    }

    async fn removed_keys_count(&self) -> Result<i64> {
        Ok(i64::try_from(self.lock().removed.len()).unwrap_or(i64::MAX))
    }

    async fn is_denied(&self, keys: &[&[u8]]) -> Result<bool> {
        Ok(Self::is_denied(&self.lock(), keys))
    }

    async fn removal_cooldowns(&self, keys: &[DhtKey]) -> Result<Vec<(DhtKey, Duration)>> {
        let state = self.lock();
        let now = Instant::now();
        Ok(keys
            .iter()
            .filter_map(|key| {
                state.removed.get(&prefix(key.as_bytes())).map(|e| {
                    // Same 7d -> 30d -> 90d schedule as the database store.
                    let days = match e.removals {
                        0 | 1 => 7,
                        2 => 30,
                        _ => 90,
                    };
                    let cooldown = Duration::from_secs(days * 24 * 60 * 60);
                    (
                        *key,
                        cooldown.saturating_sub(now.saturating_duration_since(e.removed_at)),
                    )
                })
            })
            .collect())
    }

    async fn note_removed_sightings(&self, keys: &[DhtKey]) -> Result<u64> {
        let mut state = self.lock();
        let mut n = 0u64;
        for key in keys {
            if let Some(e) = state.removed.get_mut(&prefix(key.as_bytes())) {
                e.sightings = e.sightings.saturating_add(1);
                n = n.saturating_add(1);
            }
        }
        Ok(n)
    }

    async fn refresh_scraped(&self, keys: &[DhtKey]) -> Result<u64> {
        let mut state = self.lock();
        let now = Instant::now();
        let mut n = 0u64;
        for key in keys {
            // Tombstoned and denied rows are gone from `torrents` (only a
            // fetch revives those), so only live rows refresh.
            if state.torrents.contains_key(key)
                && let Some(sc) = state.scrapes.get_mut(key)
            {
                sc.last_scraped = Some(now);
                sc.failures = 0;
                n = n.saturating_add(1);
            }
        }
        Ok(n)
    }

    async fn ping(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use dc3_store::FileRow;

    fn key(n: u8) -> DhtKey {
        DhtKey([n; 20])
    }

    fn torrent(k: DhtKey) -> NewTorrent {
        NewTorrent {
            dht_key: k,
            info_hash_v1: Some(k),
            info_hash_v2: None,
            name: "x".into(),
            total_size: 1,
            file_count: 1,
            files: vec![FileRow {
                path: "x".into(),
                size: 1,
            }],
            files_truncated: false,
            piece_length: None,
        }
    }

    fn obs(k: DhtKey, priority: bool) -> Observation {
        Observation {
            key: k,
            sightings: 1,
            priority,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn queue_lifecycle() {
        let s = MemoryStore::new();
        let out = s
            .observe(
                &[obs(key(1), false), obs(key(1), true), obs(key(2), false)],
                1,
            )
            .await
            .unwrap();
        // The queue was empty, so both are queued.
        assert_eq!(out.queued, 2);
        let out = s.observe(&[obs(key(3), false)], 1).await.unwrap();
        assert_eq!(out.dropped, 1);
        let out = s.observe(&[obs(key(3), true)], 1).await.unwrap();
        assert_eq!(out.queued, 1);

        let claimed = s.claim(10, Duration::from_secs(120)).await.unwrap();
        assert_eq!(claimed.len(), 3);
        assert_eq!(claimed[0].seen_count, 2);
        assert!(
            s.claim(10, Duration::from_secs(120))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(s.renew(&key(1), Duration::from_secs(120)).await.unwrap());
        assert_eq!(s.renewals(), 1);

        assert!(!s.fail(&key(2)).await.unwrap());
        assert_eq!(s.failures(&key(2)), 1);
        s.complete(&key(1), &torrent(key(1))).await.unwrap();
        assert!(s.torrent(&key(1)).is_some());
        assert_eq!(s.observe(&[obs(key(1), false)], 10).await.unwrap().known, 1);

        let d = s
            .deny(
                key(3).as_bytes(),
                DenyReason::CsamAuto,
                Some("n"),
                "crawler",
            )
            .await
            .unwrap();
        assert!(d.newly_denied);
        assert!(matches!(
            s.complete(&key(3), &torrent(key(3))).await,
            Err(StoreError::Denied)
        ));
        let d = s
            .deny(key(1).as_bytes(), DenyReason::Dmca, None, "cli")
            .await
            .unwrap();
        assert_eq!(d.tombstoned, 1);
        assert!(s.torrent(&key(1)).is_none());
        assert_eq!(
            s.denial(key(3).as_bytes()).unwrap().reason,
            DenyReason::CsamAuto
        );

        // Leases expire; failed keys wait for their backoff.
        tokio::time::advance(Duration::from_secs(121)).await;
        assert!(
            s.claim(10, Duration::from_secs(120))
                .await
                .unwrap()
                .is_empty()
        );
        tokio::time::advance(MEMORY_FAIL_BACKOFF).await;
        let again = s.claim(10, Duration::from_secs(120)).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].attempts, 1);

        s.fail_next_observes(1);
        assert!(s.observe(&[obs(key(9), true)], 10).await.is_err());
        assert!(s.observe(&[obs(key(9), true)], 10).await.is_ok());

        // A shorter backoff for tests.
        let quick = MemoryStore::with_fail_backoff(Duration::from_secs(1));
        quick.enqueue(key(1));
        quick.fail(&key(1)).await.unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            quick.claim(1, Duration::from_secs(1)).await.unwrap().len(),
            1
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod scrape_tests {
    use super::*;
    use tokio::time::advance;

    const LIVE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    const UNKNOWN: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    fn key(n: u8) -> DhtKey {
        DhtKey([n; 20])
    }

    fn torrent(k: DhtKey) -> NewTorrent {
        NewTorrent {
            dht_key: k,
            info_hash_v1: Some(k),
            info_hash_v2: None,
            name: "x".into(),
            total_size: 1,
            file_count: 1,
            files: vec![dc3_store::FileRow {
                path: "x".into(),
                size: 1,
            }],
            files_truncated: false,
            piece_length: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn scrape_state_round_trip() {
        let s = MemoryStore::new();
        let k = key(7);
        let id = s.complete(&k, &torrent(k)).await.unwrap();

        // Never scraped: due, and the claim holds the lease.
        let items = s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert_eq!(items[0].seeders_est, None);
        let snap = items[0].clone();
        assert!(s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap().is_empty());

        // A dead scrape is recorded.
        assert!(s.record_scrape(id, Some(0), 1).await.unwrap());
        assert!(!s.record_scrape(9999, Some(0), 1).await.unwrap());

        // A refresh bumps the guard: the stale snapshot cannot tombstone.
        s.complete(&k, &torrent(k)).await.unwrap();
        assert!(!s.tombstone_dead(id, snap.last_seen_at, snap.change_seq).await.unwrap());
        assert!(s.torrent(&k).is_some());

        // Past the unknown interval the row is due again; the fresh
        // snapshot tombstones and notes the removal.
        advance(UNKNOWN.saturating_add(Duration::from_secs(1))).await;
        let items = s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap();
        assert_eq!(items.len(), 1);
        let fresh = items[0].clone();
        assert_ne!((fresh.last_seen_at, fresh.change_seq), (snap.last_seen_at, snap.change_seq));
        assert!(s.tombstone_dead(id, fresh.last_seen_at, fresh.change_seq).await.unwrap());
        assert!(s.torrent(&k).is_none());
        assert_eq!(s.removed_keys_count().await.unwrap(), 1);
        assert!(!s.tombstone_dead(id, fresh.last_seen_at, fresh.change_seq).await.unwrap());

        // A denied tombstone is never purged.
        s.preload_denial(k.as_bytes(), DenyReason::Other);
        assert!(s.is_denied(&[k.as_bytes()]).await.unwrap());
        assert_eq!(s.purge_tombstoned(Duration::ZERO, 1000).await.unwrap(), 0);
        // Claiming skips denied rows.
        s.complete(&k, &torrent(k)).await.unwrap_err();
        assert!(s.claim_scrape_due(10, Duration::ZERO, Duration::ZERO).await.unwrap().is_empty());

        // Trimming keeps the newest rows up to the cap.
        assert_eq!(s.trim_removed_keys(10).await.unwrap(), 0);
        assert_eq!(s.trim_removed_keys(0).await.unwrap(), 1);
        assert_eq!(s.removed_keys_count().await.unwrap(), 0);
    }
}
