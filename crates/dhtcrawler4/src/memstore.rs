//! An in-memory [`CrawlStore`] for tests and experiments.
//!
//! It follows the queue rules of `dc4-store` closely enough for the pipeline
//! tests: observations queue new keys, claims lease them, `complete` stores a
//! torrent, and `fail` backs off and gives up. Nothing is persisted.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dc4_core::DhtKey;
use dc4_store::{
    MAX_COMPLETE_BATCH, MAX_FETCH_ATTEMPTS, MAX_OBSERVE_BATCH, NewTorrent, Observation,
    ObserveOutcome, PendingItem, REMOVAL_STRONG_EVIDENCE_SIGHTINGS, RemovalCooldown, Result,
    ScrapeItem, StoreError,
};
use tokio::time::Instant;

use crate::stores::CrawlStore;

/// Length of the key prefix removal memory is matched on.
const PREFIX_LEN: usize = 20;
/// Retry delay after the first failure; doubled per failure.
pub const MEMORY_FAIL_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
struct Pending {
    seen: u64,
    attempts: u32,
    next_attempt: Instant,
    lease_until: Option<Instant>,
    gave_up: bool,
    /// Last failure or give-up time, mirroring `pending.last_attempt_at`
    /// (NULL there keeps the row, so `None` here is never purged).
    last_attempt: Option<Instant>,
    /// Last piggybacked seeder estimate (`None` means unscraped).
    seeders: Option<u32>,
}

#[derive(Debug, Default)]
struct State {
    pending: BTreeMap<DhtKey, Pending>,
    torrents: HashMap<DhtKey, (i64, NewTorrent)>,
    next_id: i64,
    observe_calls: usize,
    observed: Vec<Observation>,
    claim_calls: usize,
    live_claim_calls: usize,
    failing_observes: usize,
    failing_completes: usize,
    failing_removals: usize,
    failing_batches: usize,
    failing_depths: usize,
    failing_claims: usize,
    renewals: usize,
    fails: HashMap<DhtKey, u32>,
    /// `None` means [`MEMORY_FAIL_BACKOFF`].
    fail_backoff: Option<Duration>,
    scrapes: HashMap<DhtKey, MemScrape>,
    tombstoned: HashMap<DhtKey, (Instant, i64, u64)>,
    removed: HashMap<[u8; PREFIX_LEN], RemovedEntry>,
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

/// Removal memory of one DHT key (see `removed_keys` in `dc4-store`).
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

fn prefix(key: &[u8]) -> [u8; PREFIX_LEN] {
    let mut out = [0u8; PREFIX_LEN];
    for (dst, src) in out.iter_mut().zip(key) {
        *dst = *src;
    }
    out
}

/// Mirrors the database store's base/×4/90d-cap schedule (§4a).
fn mem_cooldown(base_days: u64, removals: u32) -> Duration {
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);
    let base = DAY.saturating_mul(u32::try_from(base_days.max(1)).unwrap_or(u32::MAX));
    let cap = DAY.saturating_mul(90);
    match removals {
        0 | 1 => base.min(cap),
        2 => base.saturating_mul(4).min(cap),
        _ => cap,
    }
}

/// Remaining cooldown with the ÷4 strong-evidence shortening (sightings
/// at the threshold, or an explicit `strong` flag for seed announces) —
/// shortening, never bypassing.
fn mem_remaining(
    now: Instant,
    removed_at: Instant,
    base_days: u64,
    removals: u32,
    sightings: u32,
    strong: bool,
) -> Duration {
    let mut cooldown = mem_cooldown(base_days, removals);
    if strong || sightings >= u32::try_from(REMOVAL_STRONG_EVIDENCE_SIGHTINGS).unwrap_or(u32::MAX) {
        cooldown = cooldown.checked_div(4).unwrap_or(Duration::ZERO);
    }
    cooldown.saturating_sub(now.saturating_duration_since(removed_at))
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
            last_attempt: None,
            seeders: None,
        });
    }

    /// Number of `observe` calls, including failed ones.
    pub fn observe_calls(&self) -> usize {
        self.lock().observe_calls
    }

    /// Number of `claim` calls. The single claimer is the only caller in
    /// production, so this bounds its scan rate (no claim storm).
    pub fn claim_calls(&self) -> usize {
        self.lock().claim_calls
    }

    /// Number of `claim_live` calls (the live-first phase of each bulk).
    pub fn live_claim_calls(&self) -> usize {
        self.lock().live_claim_calls
    }

    /// Every observation of every successful `observe` call, in order.
    pub fn observed(&self) -> Vec<Observation> {
        self.lock().observed.clone()
    }

    /// Makes the next `n` `observe` calls fail.
    pub fn fail_next_observes(&self, n: usize) {
        self.lock().failing_observes = n;
    }

    /// Makes the next `n` `complete` calls fail transiently (as if the
    /// database hiccuped mid-write).
    pub fn fail_next_completes(&self, n: usize) {
        self.lock().failing_completes = n;
    }

    /// Makes the next `n` `removal_cooldowns` calls fail (as if the
    /// database hiccuped mid-read).
    pub fn fail_next_removals(&self, n: usize) {
        self.lock().failing_removals = n;
    }

    /// Makes the next `n` batched writes (`complete_batch`, `fail_batch`,
    /// `give_up_batch`) fail atomically (as if one multi-key transaction
    /// hit a transient database error). Per-key fallbacks still succeed,
    /// so workers exercise their one-key-at-a-time path.
    pub fn fail_next_batches(&self, n: usize) {
        self.lock().failing_batches = n;
    }

    /// Makes the next `n` `pending_depth` calls fail (as if the database
    /// hiccuped mid-read). Admission keeps its prior shed flag.
    pub fn fail_next_depths(&self, n: usize) {
        self.lock().failing_depths = n;
    }

    /// Makes the next `n` `claim`/`claim_live` calls fail (as if the
    /// database hiccuped mid-claim). The claimer backs off; no leases move.
    pub fn fail_next_claims(&self, n: usize) {
        self.lock().failing_claims = n;
    }

    /// Number of successful lease renewals.
    pub fn renewals(&self) -> usize {
        self.lock().renewals
    }

    /// Number of `fail` and `give_up` calls for `key`.
    pub fn failures(&self, key: &DhtKey) -> u32 {
        self.lock().fails.get(key).copied().unwrap_or(0)
    }

    /// Live queue rows with their attempt counts, for tests asserting
    /// queue invariants (no live row may sit at or past the give-up
    /// count: every fail path flips `gave_up` on the final attempt).
    /// Read-only: unlike `claim`, it sets no leases.
    pub fn live_attempts(&self) -> Vec<(DhtKey, u32)> {
        self.lock()
            .pending
            .iter()
            .filter(|(_, p)| !p.gave_up)
            .map(|(k, p)| (*k, p.attempts))
            .collect()
    }

    /// The piggybacked seeder estimate queued for `key`, if any.
    pub fn pending_seeders(&self, key: &DhtKey) -> Option<u32> {
        self.lock().pending.get(key).and_then(|p| p.seeders)
    }

    async fn claim_with(
        &self,
        n: i64,
        lease: Duration,
        live_only: bool,
    ) -> Result<Vec<PendingItem>> {
        if n <= 0 {
            return Ok(Vec::new());
        }
        let mut state = self.lock();
        state.claim_calls = state.claim_calls.saturating_add(1);
        if live_only {
            state.live_claim_calls = state.live_claim_calls.saturating_add(1);
        }
        if state.failing_claims > 0 {
            state.failing_claims = state.failing_claims.saturating_sub(1);
            return Err(StoreError::Invalid("injected claim failure".into()));
        }
        let now = Instant::now();
        let limit = usize::try_from(n.max(0)).unwrap_or(0);
        // Fresh keys first, then liveness (known-live keys), then oldest
        // attempt: mirrors CLAIM_SQL's ORDER BY attempts ASC,
        // seeders_est DESC NULLS LAST, next_attempt_at. The live phase
        // keeps only `seeders_est > 0` (mirrors CLAIM_LIVE_SQL: `Some(0)`
        // is measured dead, `None` unscraped).
        let mut due: Vec<(u32, Option<u32>, Instant, DhtKey)> = state
            .pending
            .iter()
            .filter(|(_, p)| {
                !p.gave_up
                    && p.next_attempt <= now
                    && p.lease_until.is_none_or(|l| l < now)
                    && (!live_only || p.seeders.is_some_and(|e| e > 0))
            })
            .map(|(k, p)| (p.attempts, p.seeders, p.next_attempt, *k))
            .collect();
        due.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| match (a.1, b.1) {
                    (Some(x), Some(y)) => y.cmp(&x),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.cmp(&b.3))
        });
        let mut out = Vec::new();
        for (_, _, _, key) in due.into_iter().take(limit) {
            if let Some(p) = state.pending.get_mut(&key) {
                p.lease_until = Some(now + lease);
                out.push(PendingItem {
                    dht_key: key,
                    attempts: p.attempts,
                    seen_count: p.seen,
                    seeders_est: p.seeders,
                });
            }
        }
        Ok(out)
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
        if merged.len() > MAX_OBSERVE_BATCH {
            return Err(StoreError::Invalid(format!(
                "observe batch of {} exceeds the limit of {MAX_OBSERVE_BATCH}",
                merged.len()
            )));
        }
        // Production counts every row, corpses included
        // (`Store::pending_depth`); the stand-in must gate on the same
        // number or tests never reproduce a corpse-jammed queue. The
        // double gate mirrors production: past twice the cap nothing is
        // admitted, priority included (the all-priority fast path is only
        // a saved count query with identical outcomes, so it is skipped).
        let full = i64::try_from(state.pending.len()).unwrap_or(i64::MAX) >= max_pending;
        let closed =
            i64::try_from(state.pending.len()).unwrap_or(i64::MAX) >= max_pending.saturating_mul(2);
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
            } else if let Some(p) = state.pending.get_mut(&key) {
                p.seen = p.seen.saturating_add(n);
            } else if closed || (full && !priority) {
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
                        last_attempt: None,
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
        self.claim_with(n, lease, false).await
    }

    async fn claim_live(&self, n: i64, lease: Duration) -> Result<Vec<PendingItem>> {
        self.claim_with(n, lease, true).await
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
        if state.failing_completes > 0 {
            state.failing_completes = state.failing_completes.saturating_sub(1);
            return Err(StoreError::Corrupt("injected complete failure".into()));
        }
        state.pending.remove(key);
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
        p.last_attempt = Some(now);
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
        p.last_attempt = Some(Instant::now());
        p.lease_until = None;
        p.gave_up = true;
        Ok(true)
    }

    async fn pending_depth(&self) -> Result<i64> {
        // Production counts corpses (`Store::pending_depth` documents it);
        // counting only live rows here would let tests admit into a queue
        // that production calls full.
        let mut state = self.lock();
        if state.failing_depths > 0 {
            state.failing_depths = state.failing_depths.saturating_sub(1);
            return Err(StoreError::Invalid("injected depth failure".into()));
        }
        Ok(i64::try_from(state.pending.len()).unwrap_or(i64::MAX))
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

    async fn record_scrapes(&self, rows: &[(i64, Option<u32>, u32)]) -> Result<u64> {
        let mut state = self.lock();
        let now = Instant::now();
        let mut n = 0u64;
        // Last-wins on duplicate ids, counting distinct matched rows — the
        // same contract as the SQL batch write.
        let mut merged: std::collections::BTreeMap<i64, (Option<u32>, u32)> =
            std::collections::BTreeMap::new();
        for (id, est, failures) in rows.iter().copied() {
            merged.insert(id, (est, failures));
        }
        for (id, (est, failures)) in merged {
            if let Some(sc) = state.scrapes.values_mut().find(|sc| sc.id == id) {
                sc.est = est;
                sc.failures = failures;
                sc.last_scraped = Some(now);
                n = n.saturating_add(1);
            }
        }
        Ok(n)
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
        let entry = state
            .removed
            .entry(prefix(key.as_bytes()))
            .or_insert(RemovedEntry {
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
            .filter(|(_, (at, _, _))| now.saturating_duration_since(*at) >= grace)
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

    async fn purge_gave_up(&self, older_than: Duration) -> Result<u64> {
        let mut state = self.lock();
        let now = Instant::now();
        // Mirrors the SQL predicate: only gave-up rows with a last attempt
        // older than the age go; a missing timestamp (never attempted) keeps
        // the row, as NULL does in the database.
        let doomed: Vec<DhtKey> = state
            .pending
            .iter()
            .filter(|(_, p)| {
                p.gave_up
                    && p.last_attempt
                        .is_some_and(|at| now.saturating_duration_since(at) >= older_than)
            })
            .map(|(key, _)| *key)
            .collect();
        let n = u64::try_from(doomed.len()).unwrap_or(u64::MAX);
        for key in doomed {
            state.pending.remove(&key);
        }
        Ok(n)
    }

    async fn note_fetch_estimate(&self, key: &DhtKey, seeders_est: u32) -> Result<()> {
        let mut state = self.lock();
        if let Some(p) = state.pending.get_mut(key) {
            p.seeders = Some(seeders_est);
        }
        Ok(())
    }

    async fn trim_removed_keys(&self, cap: i64) -> Result<u64> {
        if cap < 0 {
            return Ok(0);
        }
        let mut state = self.lock();
        let len = state.removed.len();
        let excess = len.saturating_sub(usize::try_from(cap.max(0)).unwrap_or(0));
        if excess == 0 {
            return Ok(0);
        }
        let mut oldest: Vec<(Instant, [u8; PREFIX_LEN])> = state
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

    async fn removal_cooldowns(
        &self,
        keys: &[DhtKey],
        base_days: u64,
        strong_evidence: &[DhtKey],
    ) -> Result<Vec<RemovalCooldown>> {
        let mut state = self.lock();
        if state.failing_removals > 0 {
            state.failing_removals = state.failing_removals.saturating_sub(1);
            return Err(StoreError::Invalid(
                "injected removal_cooldowns failure".into(),
            ));
        }
        let now = Instant::now();
        Ok(keys
            .iter()
            .filter_map(|key| {
                state.removed.get(&prefix(key.as_bytes())).map(|e| {
                    let strong = strong_evidence.contains(key);
                    RemovalCooldown {
                        key: *key,
                        remaining: mem_remaining(
                            now,
                            e.removed_at,
                            base_days,
                            e.removals,
                            e.sightings,
                            strong,
                        ),
                        sightings: e.sightings,
                    }
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
            // Tombstoned rows are gone from `torrents` (only a fetch
            // revives those), so only live rows refresh.
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

    async fn complete_batch(&self, items: &[(DhtKey, NewTorrent)]) -> Result<Vec<i64>> {
        if items.len() > MAX_COMPLETE_BATCH {
            return Err(StoreError::Invalid(format!(
                "complete batch of {} exceeds the limit of {MAX_COMPLETE_BATCH}",
                items.len()
            )));
        }
        {
            let mut state = self.lock();
            if state.failing_batches > 0 {
                state.failing_batches = state.failing_batches.saturating_sub(1);
                return Err(StoreError::Corrupt("injected batch failure".into()));
            }
        }
        let mut ids = Vec::with_capacity(items.len());
        for (key, t) in items {
            ids.push(self.complete(key, t).await?);
        }
        Ok(ids)
    }

    async fn fail_batch(&self, keys: &[DhtKey]) -> Result<()> {
        {
            let mut state = self.lock();
            if state.failing_batches > 0 {
                state.failing_batches = state.failing_batches.saturating_sub(1);
                return Err(StoreError::Corrupt("injected batch failure".into()));
            }
        }
        for key in keys {
            self.fail(key).await?;
        }
        Ok(())
    }

    async fn give_up_batch(&self, keys: &[DhtKey]) -> Result<()> {
        {
            let mut state = self.lock();
            if state.failing_batches > 0 {
                state.failing_batches = state.failing_batches.saturating_sub(1);
                return Err(StoreError::Corrupt("injected batch failure".into()));
            }
        }
        for key in keys {
            self.give_up(key).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use dc4_store::FileRow;

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
    async fn oversized_batches_fail_loudly() {
        let s = MemoryStore::new();
        let big_key = |i: usize| {
            let mut b = [0u8; 20];
            b[..8].copy_from_slice(&(i as u64).to_be_bytes());
            DhtKey(b)
        };
        let big: Vec<(DhtKey, NewTorrent)> = (0..MAX_COMPLETE_BATCH + 1)
            .map(|i| {
                let k = big_key(i);
                (k, torrent(k))
            })
            .collect();
        assert!(matches!(
            s.complete_batch(&big).await,
            Err(StoreError::Invalid(_))
        ));
        let many: Vec<Observation> = (0..MAX_OBSERVE_BATCH + 1)
            .map(|i| obs(big_key(i), false))
            .collect();
        assert!(matches!(
            s.observe(&many, i64::MAX).await,
            Err(StoreError::Invalid(_))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn queue_lifecycle() {
        let s = MemoryStore::new();
        // No pool behind the stand-in, so no pool gauges to export.
        assert!(s.pool_status().is_none());
        let out = s
            .observe(
                &[obs(key(1), false), obs(key(1), true), obs(key(2), false)],
                2,
            )
            .await
            .unwrap();
        // The queue was empty, so both are queued.
        assert_eq!(out.queued, 2);
        let out = s.observe(&[obs(key(3), false)], 2).await.unwrap();
        assert_eq!(out.dropped, 1);
        let out = s.observe(&[obs(key(3), true)], 2).await.unwrap();
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
        s.complete(&key(3), &torrent(key(3))).await.unwrap();
        assert!(s.torrent(&key(3)).is_some());

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

    /// Gave-up rows count toward the cap, like production: corpses jam
    /// the queue until the purge removes them.
    #[tokio::test(start_paused = true)]
    async fn gave_up_keys_count_toward_the_cap_until_purged() {
        let s = MemoryStore::new();
        s.observe(&[obs(key(1), false), obs(key(2), false)], 10)
            .await
            .unwrap();
        assert!(s.give_up(&key(1)).await.unwrap());
        assert!(s.give_up(&key(2)).await.unwrap());
        // Nothing claimable remains, but the corpses still count.
        assert!(s.pending_keys().is_empty());
        assert_eq!(s.pending_depth().await.unwrap(), 2);
        // At the cap a new key is dropped, not queued.
        let out = s.observe(&[obs(key(3), false)], 1).await.unwrap();
        assert_eq!(out.queued, 0);
        assert_eq!(out.dropped, 1);
        assert_eq!(s.pending_depth().await.unwrap(), 2);
        // The purge reopens the queue: corpses leave, the key is accepted.
        assert_eq!(s.purge_gave_up(Duration::ZERO).await.unwrap(), 2);
        assert_eq!(s.pending_depth().await.unwrap(), 0);
        let out = s.observe(&[obs(key(3), false)], 1).await.unwrap();
        assert_eq!(out.queued, 1);
        assert_eq!(out.dropped, 0);
        assert_eq!(s.pending_depth().await.unwrap(), 1);
    }

    /// Past twice the cap nothing is admitted, priority included
    /// (production `Closed`); between one and twice only priority passes.
    #[tokio::test(start_paused = true)]
    async fn closed_queue_drops_priority_keys_like_production() {
        let s = MemoryStore::new();
        s.observe(&[obs(key(51), false), obs(key(52), false)], 10)
            .await
            .unwrap();
        // Twice the cap: sampler and announce keys drop like get_peers ones.
        let out = s
            .observe(&[obs(key(53), true), obs(key(54), false)], 1)
            .await
            .unwrap();
        assert_eq!(out.queued, 0);
        assert_eq!(out.dropped, 2);
        assert_eq!(s.pending_depth().await.unwrap(), 2);
        // Between one and twice the cap only priority gets through.
        let out = s
            .observe(&[obs(key(55), true), obs(key(56), false)], 2)
            .await
            .unwrap();
        assert_eq!(out.queued, 1);
        assert_eq!(out.dropped, 1);
        assert_eq!(s.pending_keys(), vec![key(51), key(52), key(55)]);
        assert_eq!(s.pending_depth().await.unwrap(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_prefers_fresh_keys_over_retried() {
        let s = MemoryStore::with_fail_backoff(Duration::from_secs(1));
        s.observe(&[obs(key(1), false), obs(key(2), false)], 10)
            .await
            .unwrap();
        // Key 2 failed once but carries a seeder estimate; key 1 is fresh.
        s.note_fetch_estimate(&key(2), 50).await.unwrap();
        s.fail(&key(2)).await.unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        let one = s.claim(1, Duration::from_secs(60)).await.unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].dht_key, key(1));
    }

    #[tokio::test(start_paused = true)]
    async fn claim_carries_the_piggybacked_estimate() {
        let s = MemoryStore::new();
        s.observe(&[obs(key(1), false), obs(key(2), false)], 10)
            .await
            .unwrap();
        s.note_fetch_estimate(&key(1), 7).await.unwrap();
        let claimed = s.claim(10, Duration::from_secs(120)).await.unwrap();
        assert_eq!(claimed.len(), 2);
        let est = |k: DhtKey| claimed.iter().find(|i| i.dht_key == k).unwrap().seeders_est;
        assert_eq!(est(key(1)), Some(7));
        assert_eq!(est(key(2)), None);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_live_takes_only_live_ordered_by_estimate() {
        let s = MemoryStore::new();
        s.observe(
            &[
                obs(key(1), false),
                obs(key(2), false),
                obs(key(3), false),
                obs(key(4), false),
            ],
            10,
        )
        .await
        .unwrap();
        s.note_fetch_estimate(&key(1), 3).await.unwrap();
        s.note_fetch_estimate(&key(2), 50).await.unwrap();
        // Measured dead (aware, no seeds) — never live.
        s.note_fetch_estimate(&key(3), 0).await.unwrap();
        // key(4) stays NULL (unscraped).
        let live = s.claim_live(10, Duration::from_secs(120)).await.unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].dht_key, key(2));
        assert_eq!(live[1].dht_key, key(1));
        assert!(live.iter().all(|i| i.seeders_est.is_some_and(|e| e > 0)));
        // Leased by the live phase: the follow-up unfiltered claim cannot
        // re-lease them, so the pair never double-leases.
        let rest = s.claim(10, Duration::from_secs(120)).await.unwrap();
        assert_eq!(rest.len(), 2);
        assert!(rest.iter().all(|i| !i.seeders_est.is_some_and(|e| e > 0)));
        assert_eq!(s.live_claim_calls(), 1);
        assert_eq!(s.claim_calls(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_live_empty_pool_leaves_everything_for_the_top_up() {
        let s = MemoryStore::new();
        s.observe(&[obs(key(1), false), obs(key(2), false)], 10)
            .await
            .unwrap();
        assert!(
            s.claim_live(10, Duration::from_secs(120))
                .await
                .unwrap()
                .is_empty()
        );
        let rest = s.claim(10, Duration::from_secs(120)).await.unwrap();
        assert_eq!(rest.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_rejects_nonpositive_n_without_a_scan() {
        let s = MemoryStore::new();
        s.observe(&[obs(key(1), false)], 10).await.unwrap();
        assert!(
            s.claim(0, Duration::from_secs(120))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            s.claim_live(0, Duration::from_secs(120))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            s.claim(-5, Duration::from_secs(120))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(s.claim_calls(), 0);
        assert_eq!(s.live_claim_calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn claim_failures_move_no_leases() {
        let s = MemoryStore::new();
        s.observe(&[obs(key(1), false)], 10).await.unwrap();
        s.fail_next_claims(2);
        assert!(s.claim_live(10, Duration::from_secs(120)).await.is_err());
        assert!(s.claim(10, Duration::from_secs(120)).await.is_err());
        // Nothing was leased: the next try claims the key.
        let claimed = s.claim(10, Duration::from_secs(120)).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].dht_key, key(1));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod scrape_tests {
    use super::*;
    use tokio::time::advance;

    const LIVE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    const UNKNOWN: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    const HOUR: Duration = Duration::from_secs(3600);

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
            files: vec![dc4_store::FileRow {
                path: "x".into(),
                size: 1,
            }],
            files_truncated: false,
            piece_length: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn removal_cooldown_respects_base_and_shortens_on_evidence() {
        use std::time::Duration as StdDuration;

        let s = MemoryStore::new();
        let k = key(9);
        let id = s.complete(&k, &torrent(k)).await.unwrap();
        let items = s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap();
        let snap = items.iter().find(|c| c.id == id).unwrap().clone();
        assert!(
            s.tombstone_dead(id, snap.last_seen_at, snap.change_seq)
                .await
                .unwrap()
        );

        let day = StdDuration::from_secs(24 * 60 * 60);
        // Base 7: blocked ~7d; base 1: blocked ~1d, not 7d.
        let rows = s.removal_cooldowns(&[k], 7, &[]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].remaining > day * 6 && rows[0].remaining <= day * 7);
        let rows = s.removal_cooldowns(&[k], 1, &[]).await.unwrap();
        assert!(rows[0].remaining > StdDuration::ZERO && rows[0].remaining <= day);

        // Three sightings: ÷4 (7d → 1.75d), still blocking while fresh.
        for _ in 0..3 {
            assert_eq!(s.note_removed_sightings(&[k]).await.unwrap(), 1);
        }
        let rows = s.removal_cooldowns(&[k], 7, &[]).await.unwrap();
        assert_eq!(rows[0].sightings, 3);
        assert!(rows[0].remaining > StdDuration::ZERO && rows[0].remaining <= day * 2);

        // A seed announce shortens the same way, without any sightings.
        let other = key(10);
        let oid = s.complete(&other, &torrent(other)).await.unwrap();
        let items = s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap();
        let snap = items.iter().find(|c| c.id == oid).unwrap().clone();
        assert!(
            s.tombstone_dead(oid, snap.last_seen_at, snap.change_seq)
                .await
                .unwrap()
        );
        let rows = s.removal_cooldowns(&[other], 7, &[other]).await.unwrap();
        assert!(rows[0].remaining > StdDuration::ZERO && rows[0].remaining <= day * 2);

        // Two days later the shortened cooldown has expired while the
        // unshortened one still blocks: shortening, never bypassing.
        // (`k` carries its sighting shortening intrinsically; `other`
        // needs the explicit seed evidence.)
        advance(day * 2).await;
        let rows = s.removal_cooldowns(&[k, other], 7, &[]).await.unwrap();
        let rk = rows.iter().find(|r| r.key == k).unwrap();
        let ro = rows.iter().find(|r| r.key == other).unwrap();
        assert_eq!(rk.remaining, StdDuration::ZERO);
        assert!(ro.remaining > day * 4);
        let rows = s.removal_cooldowns(&[other], 7, &[other]).await.unwrap();
        assert_eq!(rows[0].remaining, StdDuration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn record_scrapes_batches_stats_only_writes() {
        let s = MemoryStore::new();
        let mut ids = Vec::new();
        for n in [11u8, 12] {
            let k = key(n);
            ids.push(s.complete(&k, &torrent(k)).await.unwrap());
        }
        let n = s
            .record_scrapes(&[(ids[0], Some(3), 0), (ids[1], None, 1), (9999, Some(1), 0)])
            .await
            .unwrap();
        assert_eq!(n, 2, "unknown ids match nothing");
        assert_eq!(s.record_scrapes(&[]).await.unwrap(), 0);
        // Unknown outcomes keep the old estimate and failures.
        let items = s.claim_scrape_due(10, LIVE, LIVE).await.unwrap();
        assert!(items.is_empty(), "claims just stamped both rows");
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
        assert!(
            s.claim_scrape_due(10, LIVE, UNKNOWN)
                .await
                .unwrap()
                .is_empty()
        );

        // A dead scrape is recorded.
        assert!(s.record_scrape(id, Some(0), 1).await.unwrap());
        assert!(!s.record_scrape(9999, Some(0), 1).await.unwrap());

        // A refresh bumps the guard: the stale snapshot cannot tombstone.
        s.complete(&k, &torrent(k)).await.unwrap();
        assert!(
            !s.tombstone_dead(id, snap.last_seen_at, snap.change_seq)
                .await
                .unwrap()
        );
        assert!(s.torrent(&k).is_some());

        // Past the unknown interval the row is due again; the fresh
        // snapshot tombstones and notes the removal.
        advance(UNKNOWN.saturating_add(Duration::from_secs(1))).await;
        let items = s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap();
        assert_eq!(items.len(), 1);
        let fresh = items[0].clone();
        assert_ne!(
            (fresh.last_seen_at, fresh.change_seq),
            (snap.last_seen_at, snap.change_seq)
        );
        assert!(
            s.tombstone_dead(id, fresh.last_seen_at, fresh.change_seq)
                .await
                .unwrap()
        );
        assert!(s.torrent(&k).is_none());
        assert_eq!(s.removed_keys_count().await.unwrap(), 1);
        assert!(
            !s.tombstone_dead(id, fresh.last_seen_at, fresh.change_seq)
                .await
                .unwrap()
        );

        // A fresh tombstone purges once its grace expires.
        assert_eq!(s.purge_tombstoned(Duration::ZERO, 1000).await.unwrap(), 1);

        // Trimming keeps the newest rows up to the cap.
        assert_eq!(s.trim_removed_keys(10).await.unwrap(), 0);
        assert_eq!(s.trim_removed_keys(0).await.unwrap(), 1);
        assert_eq!(s.removed_keys_count().await.unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn trim_negative_cap_keeps_everything() {
        let s = MemoryStore::new();
        let k = key(21);
        let id = s.complete(&k, &torrent(k)).await.unwrap();
        let items = s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap();
        let snap = items.iter().find(|c| c.id == id).unwrap().clone();
        assert!(
            s.tombstone_dead(id, snap.last_seen_at, snap.change_seq)
                .await
                .unwrap()
        );
        assert_eq!(s.removed_keys_count().await.unwrap(), 1);
        assert_eq!(s.trim_removed_keys(-5).await.unwrap(), 0);
        assert_eq!(s.removed_keys_count().await.unwrap(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn observe_requeues_tombstoned_for_refetch() {
        let s = MemoryStore::new();
        let k = key(22);
        let id = s.complete(&k, &torrent(k)).await.unwrap();
        let items = s.claim_scrape_due(10, LIVE, UNKNOWN).await.unwrap();
        let snap = items.iter().find(|c| c.id == id).unwrap().clone();
        assert!(
            s.tombstone_dead(id, snap.last_seen_at, snap.change_seq)
                .await
                .unwrap()
        );
        // The dead row is untouched but the key re-queues, so a refetch can
        // revive it once admission lets it through.
        let out = s
            .observe(
                &[Observation {
                    key: k,
                    sightings: 5,
                    priority: false,
                }],
                i64::MAX,
            )
            .await
            .unwrap();
        assert_eq!((out.known, out.queued, out.dropped), (0, 1, 0));
        assert!(s.torrent(&k).is_none());
        assert_eq!(s.pending_keys(), vec![k]);
    }

    /// `MAX_FETCH_ATTEMPTS` failures give up; only gave-up rows older than
    /// the age purge, mirroring the SQL predicate.
    #[tokio::test(start_paused = true)]
    async fn purge_gave_up_keeps_young_and_live_rows() {
        let s = MemoryStore::new();
        for k in [key(31), key(32), key(33)] {
            s.enqueue(k);
        }
        s.fail(&key(31)).await.unwrap();
        assert!(!s.lock().pending[&key(31)].gave_up);
        s.fail(&key(31)).await.unwrap();
        assert!(s.lock().pending[&key(31)].gave_up);
        s.fail(&key(32)).await.unwrap();
        assert!(!s.lock().pending[&key(32)].gave_up);

        // Nothing is old enough for an hour age.
        assert_eq!(s.purge_gave_up(HOUR).await.unwrap(), 0);
        assert!(s.lock().pending.contains_key(&key(31)));

        // Past the age only the gave-up row goes; the live retry-waiting
        // row and the never-attempted row stay.
        tokio::time::advance(HOUR.saturating_add(Duration::from_secs(1))).await;
        assert_eq!(s.purge_gave_up(HOUR).await.unwrap(), 1);
        assert!(!s.lock().pending.contains_key(&key(31)));
        assert!(s.lock().pending.contains_key(&key(32)));
        assert!(s.lock().pending.contains_key(&key(33)));
        // A second purge is a no-op.
        assert_eq!(s.purge_gave_up(Duration::ZERO).await.unwrap(), 0);
    }

    /// Direct give-ups stamp the attempt time too: a zero age purges them.
    /// A gave-up row with no stamp is kept, as NULL is in the database.
    #[tokio::test(start_paused = true)]
    async fn purge_gave_up_covers_direct_give_ups() {
        let s = MemoryStore::new();
        s.enqueue(key(34));
        s.give_up(&key(34)).await.unwrap();
        assert_eq!(s.purge_gave_up(Duration::ZERO).await.unwrap(), 1);
        assert!(!s.lock().pending.contains_key(&key(34)));

        s.lock().pending.insert(
            key(35),
            Pending {
                seen: 1,
                attempts: 2,
                next_attempt: tokio::time::Instant::now(),
                lease_until: None,
                gave_up: true,
                last_attempt: None,
                seeders: None,
            },
        );
        assert_eq!(s.purge_gave_up(Duration::ZERO).await.unwrap(), 0);
        assert!(s.lock().pending.contains_key(&key(35)));
    }
}
