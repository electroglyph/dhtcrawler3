//! An in-memory [`CrawlStore`] for tests and experiments.
//!
//! It follows the queue rules of `dc3-store` closely enough for the pipeline
//! tests: observations queue new keys, claims lease them, `complete` stores a
//! torrent unless a key is denied, `fail` backs off and gives up, and `deny`
//! tombstones. Nothing is persisted.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dc3_core::DhtKey;
use dc3_store::{
    DenyOutcome, DenyReason, MAX_FETCH_ATTEMPTS, NewTorrent, Observation, ObserveOutcome,
    PendingItem, Result, StoreError,
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
        let mut due: Vec<(Instant, DhtKey)> = state
            .pending
            .iter()
            .filter(|(_, p)| {
                !p.gave_up && p.next_attempt <= now && p.lease_until.is_none_or(|l| l < now)
            })
            .map(|(k, p)| (p.next_attempt, *k))
            .collect();
        due.sort();
        let mut out = Vec::new();
        for (_, key) in due.into_iter().take(limit) {
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
