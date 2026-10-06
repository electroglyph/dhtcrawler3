//! Admission (design §13): turns DHT discoveries into queue entries.
//!
//! 1. A discovered `peer` passes the address chokepoint and goes into the
//!    [`HintMap`].
//! 2. A key already in the [`DedupSet`] is skipped.
//! 3. A key seen only through `get_peers` is admitted once sightings came
//!    from at least two /24 (IPv4) or /48 (IPv6) networks within the current
//!    dedup generation ([`SourceTracker`]). Sampled and announced keys are
//!    admitted at once, as priority keys.
//! 4. Admitted keys are merged into a batch that is written with
//!    `observe` every second or every 1 000 keys, retrying with backoff.
//! 5. A key enters the dedup set only after the `observe` call that
//!    included it succeeded.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dc3_core::DhtKey;
use dc3_dht::compact::canonical_ip;
use dc3_dht::{Discovered, Family, Source};
use dc3_store::{Observation, ObserveOutcome};
use lru::LruCache;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::peers::{OwnAddrs, PeerFilter, PeerSource};
use crate::stores::CrawlStore;

/// Capacity of the channel from the DHT node to admission.
pub const DISCOVERED_CHANNEL_CAPACITY: usize = 65_536;
/// Keys per dedup generation.
pub const DEDUP_GENERATION_KEYS: usize = 2_000_000;
/// Age at which the dedup set starts a new generation.
pub const DEDUP_ROTATION: Duration = Duration::from_secs(30 * 60);
/// Distinct source networks a `get_peers`-only key needs.
pub const GET_PEERS_MIN_SOURCES: usize = 2;
/// Keys the source tracker remembers.
pub const SOURCE_TRACKER_KEYS: usize = 200_000;
/// Source networks remembered per key.
pub const SOURCE_PREFIXES_PER_KEY: usize = 4;
/// IPv4 source network size (bits).
pub const SOURCE_PREFIX_V4: u32 = 24;
/// IPv6 source network size (bits).
pub const SOURCE_PREFIX_V6: u32 = 48;
/// Keys the hint map remembers.
pub const HINT_KEYS: usize = 100_000;
/// Peers remembered per key in the hint map.
pub const HINT_PEERS_PER_KEY: usize = 8;
/// Lifetime of a hint.
pub const HINT_TTL: Duration = Duration::from_secs(5 * 60);
/// Keys the removal-memory cache holds; beyond this it is cleared and
/// rebuilt from the database (a documented heuristic: cooldowns only
/// grow the cache under flap).
pub const REMOVAL_CACHE_KEYS: usize = 100_000;
/// How long a "not removed" answer is trusted without re-checking.
pub const REMOVAL_CACHE_CLEAR_TTL: Duration = Duration::from_secs(60 * 60);
/// Keys that trigger an immediate batch write.
pub const BATCH_MAX_KEYS: usize = 1_000;
/// Longest time a batch waits before it is written.
pub const BATCH_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
/// First retry delay after a failed batch write.
pub const FLUSH_RETRY_BASE: Duration = Duration::from_millis(500);
/// Longest retry delay after failed batch writes.
pub const FLUSH_RETRY_MAX: Duration = Duration::from_secs(30);
/// How long the final flush may keep retrying at shutdown.
pub const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// How often our own addresses are re-read from the node.
pub const OWN_ADDRS_REFRESH: Duration = Duration::from_secs(5);
/// Shortest flush period accepted (a zero period would spin).
const MIN_FLUSH_INTERVAL: Duration = Duration::from_millis(1);

const METRIC_DISCOVERED: &str = "dc3_discovered_total";
const METRIC_ADMITTED: &str = "dc3_admitted_total";
/// Pipeline blocks, by reason (also used by `fetch`).
pub const METRIC_BLOCKED: &str = "dc3_blocked_total";

const fn non_zero(n: usize) -> NonZeroUsize {
    match NonZeroUsize::new(n) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Timings and sizes of admission. `Default` gives the production values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionTuning {
    pub dedup_keys: usize,
    pub dedup_rotation: Duration,
    pub source_keys: usize,
    pub hint_keys: usize,
    pub hint_ttl: Duration,
    pub batch_max_keys: usize,
    pub flush_interval: Duration,
    pub retry_base: Duration,
    pub retry_max: Duration,
    pub shutdown_flush_timeout: Duration,
}

impl Default for AdmissionTuning {
    fn default() -> Self {
        Self {
            dedup_keys: DEDUP_GENERATION_KEYS,
            dedup_rotation: DEDUP_ROTATION,
            source_keys: SOURCE_TRACKER_KEYS,
            hint_keys: HINT_KEYS,
            hint_ttl: HINT_TTL,
            batch_max_keys: BATCH_MAX_KEYS,
            flush_interval: BATCH_FLUSH_INTERVAL,
            retry_base: FLUSH_RETRY_BASE,
            retry_max: FLUSH_RETRY_MAX,
            shutdown_flush_timeout: SHUTDOWN_FLUSH_TIMEOUT,
        }
    }
}

// ---------------------------------------------------------------------------
// Dedup set
// ---------------------------------------------------------------------------

/// A hasher for values that are already uniformly distributed `u64`s.
#[derive(Default)]
struct PassThroughHasher(u64);

impl Hasher for PassThroughHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*b);
        }
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

type FingerprintSet = HashSet<u64, BuildHasherDefault<PassThroughHasher>>;

/// Keys observed recently, in two generations.
///
/// Keys are kept as 64-bit fingerprints under a per-process random key, so a
/// generation of 2 000 000 keys takes tens of megabytes, and nobody can
/// choose keys that collide with a given one. A collision only delays a
/// key until the next rotation.
pub struct DedupSet {
    fingerprint: RandomState,
    current: FingerprintSet,
    previous: FingerprintSet,
    capacity: usize,
    rotate_every: Duration,
    rotated_at: Instant,
}

impl std::fmt::Debug for DedupSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DedupSet")
            .field("current", &self.current.len())
            .field("previous", &self.previous.len())
            .finish_non_exhaustive()
    }
}

impl DedupSet {
    /// An empty set whose first generation starts at `now`.
    pub fn new(capacity: usize, rotate_every: Duration, now: Instant) -> Self {
        Self {
            fingerprint: RandomState::new(),
            current: FingerprintSet::default(),
            previous: FingerprintSet::default(),
            capacity: capacity.max(1),
            rotate_every,
            rotated_at: now,
        }
    }

    fn fingerprint(&self, key: &DhtKey) -> u64 {
        self.fingerprint.hash_one(key.0)
    }

    /// Whether `key` is in either generation.
    pub fn contains(&self, key: &DhtKey) -> bool {
        let fp = self.fingerprint(key);
        self.current.contains(&fp) || self.previous.contains(&fp)
    }

    /// Adds `key`. Returns true if a new generation started because the
    /// current one was full.
    pub fn insert(&mut self, key: &DhtKey, now: Instant) -> bool {
        let fp = self.fingerprint(key);
        if self.current.contains(&fp) {
            return false;
        }
        let rotated = self.current.len() >= self.capacity;
        if rotated {
            self.rotate(now);
        }
        self.current.insert(fp);
        rotated
    }

    /// Starts a new generation if the current one is old enough. Returns
    /// true if it did.
    pub fn maybe_rotate(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.rotated_at) < self.rotate_every {
            return false;
        }
        self.rotate(now);
        true
    }

    fn rotate(&mut self, now: Instant) {
        self.previous = std::mem::take(&mut self.current);
        self.rotated_at = now;
    }

    /// Keys in the current and previous generations.
    pub fn len(&self) -> (usize, usize) {
        (self.current.len(), self.previous.len())
    }

    /// True when both generations are empty.
    pub fn is_empty(&self) -> bool {
        self.current.is_empty() && self.previous.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Source tracker
// ---------------------------------------------------------------------------

/// The network a source address belongs to, as one number: /24 for IPv4,
/// /48 for IPv6 (IPv4-mapped addresses count as IPv4).
pub fn source_network(ip: IpAddr) -> u64 {
    const V6_SHIFT: u32 = 128 - SOURCE_PREFIX_V6;
    const V4_SHIFT: u32 = 32 - SOURCE_PREFIX_V4;
    const V4_TAG: u64 = 1 << SOURCE_PREFIX_V6;
    match canonical_ip(ip) {
        // An IPv4 network is at least V4_TAG; an IPv6 one (48 bits) is below it.
        IpAddr::V4(v4) => V4_TAG | u64::from(u32::from(v4) >> V4_SHIFT),
        IpAddr::V6(v6) => u64::try_from(u128::from(v6) >> V6_SHIFT).unwrap_or(u64::MAX),
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Networks {
    seen: [u64; SOURCE_PREFIXES_PER_KEY],
    len: usize,
}

impl Networks {
    fn add(&mut self, network: u64) -> usize {
        let known = self.seen.iter().take(self.len).any(|n| *n == network);
        if !known && let Some(slot) = self.seen.get_mut(self.len) {
            *slot = network;
            self.len = self.len.saturating_add(1);
        }
        self.len
    }
}

/// For keys seen only through `get_peers`, the distinct source networks
/// seen so far (bounded LRU).
pub struct SourceTracker {
    map: LruCache<DhtKey, Networks>,
}

impl std::fmt::Debug for SourceTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceTracker")
            .field("keys", &self.map.len())
            .finish()
    }
}

impl SourceTracker {
    /// A tracker for at most `keys` keys.
    pub fn new(keys: usize) -> Self {
        Self {
            map: LruCache::new(non_zero(keys)),
        }
    }

    /// Records a sighting of `key` from `from`; returns the number of
    /// distinct networks seen for it.
    pub fn record(&mut self, key: DhtKey, from: IpAddr) -> usize {
        let network = source_network(from);
        self.map
            .get_or_insert_mut(key, Networks::default)
            .add(network)
    }

    /// Forgets `key`.
    pub fn remove(&mut self, key: &DhtKey) {
        self.map.pop(key);
    }

    /// Forgets everything (a new dedup generation began).
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Keys tracked.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Hint map
// ---------------------------------------------------------------------------

/// Peers that announced a key, kept briefly so a fetch can try them before
/// (and besides) a `get_peers` lookup. Memory only (R11).
pub struct HintMap {
    map: LruCache<DhtKey, Vec<(SocketAddr, Instant)>>,
    ttl: Duration,
}

impl std::fmt::Debug for HintMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HintMap")
            .field("keys", &self.map.len())
            .finish_non_exhaustive()
    }
}

impl HintMap {
    /// A map for at most `keys` keys whose hints expire after `ttl`.
    pub fn new(keys: usize, ttl: Duration) -> Self {
        Self {
            map: LruCache::new(non_zero(keys)),
            ttl,
        }
    }

    /// Remembers `peer` for `key`. The caller has already filtered it.
    pub fn add(&mut self, key: DhtKey, peer: SocketAddr, now: Instant) {
        let ttl = self.ttl;
        let peers = self.map.get_or_insert_mut(key, Vec::new);
        peers.retain(|(p, at)| *p != peer && now.saturating_duration_since(*at) < ttl);
        // `retain` above removed any older entry for this peer; drop the
        // oldest others until there is room (the vector is non-empty here).
        while peers.len() >= HINT_PEERS_PER_KEY {
            peers.remove(0);
        }
        peers.push((peer, now));
    }

    /// Removes and returns the unexpired hints for `key`, newest first.
    pub fn take(&mut self, key: &DhtKey, now: Instant) -> Vec<SocketAddr> {
        let ttl = self.ttl;
        self.map
            .pop(key)
            .unwrap_or_default()
            .into_iter()
            .rev()
            .filter(|(_, at)| now.saturating_duration_since(*at) < ttl)
            .map(|(p, _)| p)
            .collect()
    }

    /// Keys with hints.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when there are no hints.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The hint map shared by admission and the fetch workers.
pub type SharedHints = Arc<Mutex<HintMap>>;

/// A new shared hint map.
pub fn shared_hints(tuning: &AdmissionTuning) -> SharedHints {
    Arc::new(Mutex::new(HintMap::new(tuning.hint_keys, tuning.hint_ttl)))
}

/// Takes the hints for `key` from a shared map.
pub fn take_hints(hints: &SharedHints, key: &DhtKey, now: Instant) -> Vec<SocketAddr> {
    lock(hints).take(key, now)
}

// ---------------------------------------------------------------------------
// Admission
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Pending {
    sightings: u32,
    priority: bool,
}

/// Why a batch was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushStopped {
    /// Shutdown was requested while retrying.
    Cancelled,
    /// The deadline passed while retrying.
    Deadline,
}

/// The admission state: dedup set, source tracker, hints and the batch.
pub struct Admission<S> {
    store: S,
    max_pending: i64,
    filter: PeerFilter,
    tuning: AdmissionTuning,
    dedup: DedupSet,
    sources: SourceTracker,
    hints: SharedHints,
    batch: HashMap<DhtKey, Pending>,
    /// Keys announced with `seed=1` since the last flush: live seeders that
    /// short-circuit their next scrape (bep33.md §4b win 6).
    seed_announced: HashSet<DhtKey>,
    /// Removal-memory verdicts by key (bep33.md §4a): a synchronous PK
    /// lookup per discovery would kill throughput, so verdicts are cached
    /// here and refreshed in batch by [`Admission::flush`].
    removal_cache: HashMap<DhtKey, RemovalVerdict>,
}

/// A cached removal-memory verdict.
#[derive(Debug, Clone, Copy)]
enum RemovalVerdict {
    /// In cooldown until the instant.
    Blocked(Instant),
    /// Known-clear when checked at the instant.
    Clear(Instant),
}

impl<S> std::fmt::Debug for Admission<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admission")
            .field("batch", &self.batch.len())
            .field("dedup", &self.dedup)
            .finish_non_exhaustive()
    }
}

impl<S: CrawlStore> Admission<S> {
    /// New admission state writing to `store`.
    pub fn new(
        store: S,
        max_pending: i64,
        filter: PeerFilter,
        tuning: AdmissionTuning,
        hints: SharedHints,
        now: Instant,
    ) -> Self {
        Self {
            store,
            max_pending,
            filter,
            tuning,
            dedup: DedupSet::new(tuning.dedup_keys, tuning.dedup_rotation, now),
            sources: SourceTracker::new(tuning.source_keys),
            hints,
            batch: HashMap::new(),
            seed_announced: HashSet::new(),
            removal_cache: HashMap::new(),
        }
    }

    /// Keys waiting in the batch.
    pub fn batch_len(&self) -> usize {
        self.batch.len()
    }

    /// The dedup set.
    pub fn dedup(&self) -> &DedupSet {
        &self.dedup
    }

    /// The source tracker.
    pub fn sources(&self) -> &SourceTracker {
        &self.sources
    }

    /// Handles one discovery.
    pub fn handle(&mut self, event: Discovered, own: &OwnAddrs, now: Instant) {
        let source = event.source;
        metrics::counter!(
            METRIC_DISCOVERED,
            "source" => source.as_str(),
            "family" => Family::of_ip(&canonical_ip(event.from)).as_str()
        )
        .increment(1);

        if let Some(peer) = event.peer {
            match self.filter.accept(peer, own) {
                Some(peer) => lock(&self.hints).add(event.key, peer, now),
                None => {
                    metrics::counter!(METRIC_BLOCKED, "reason" => "peer_address").increment(1);
                }
            }
        }

        if self.dedup.maybe_rotate(now) {
            self.sources.clear();
        }
        if event.seed {
            // A live seeder just proved itself: refresh its scrape clock
            // at flush time (announce short-circuit, §4b win 6). The key
            // still flows through the normal batch path below (its peer
            // is a hint like any other).
            self.seed_announced.insert(event.key);
        }
        if self.dedup.contains(&event.key) {
            return;
        }
        let priority = source != Source::GetPeers;
        if let Some(pending) = self.batch.get_mut(&event.key) {
            pending.sightings = pending.sightings.saturating_add(1);
            pending.priority |= priority;
            return;
        }
        if priority {
            self.sources.remove(&event.key);
        } else {
            if self.sources.record(event.key, event.from) < GET_PEERS_MIN_SOURCES {
                return;
            }
            self.sources.remove(&event.key);
        }
        self.batch.insert(
            event.key,
            Pending {
                sightings: 1,
                priority,
            },
        );
        metrics::counter!(METRIC_ADMITTED, "source" => source.as_str()).increment(1);
    }

    fn observations(&self) -> Vec<Observation> {
        self.batch
            .iter()
            .map(|(key, p)| Observation {
                key: *key,
                sightings: p.sightings,
                priority: p.priority,
            })
            .collect()
    }

    /// Drops batch keys still in removal cooldown (§4a) and counts a
    /// post-removal sighting for every batch key the memory still knows.
    /// Verdicts come from the LRU [`REMOVAL_CACHE_KEYS`] cache; misses are
    /// checked in one batched query.
    async fn gate_removals(&mut self) {
        let now = Instant::now();
        let mut misses = Vec::new();
        let mut blocked_keys = Vec::new();
        let mut remembered = Vec::new();
        for key in self.batch.keys().copied().collect::<Vec<_>>() {
            match self.removal_cache.get(&key) {
                Some(RemovalVerdict::Blocked(until)) if *until > now => {
                    blocked_keys.push(key);
                    remembered.push(key);
                }
                Some(RemovalVerdict::Clear(at)) if now < *at + REMOVAL_CACHE_CLEAR_TTL => {}
                _ => misses.push(key),
            }
        }
        if !misses.is_empty() {
            match self.store.removal_cooldowns(&misses).await {
                Ok(rows) => {
                    for (key, remaining) in rows {
                        if remaining > Duration::ZERO {
                            self.removal_cache
                                .insert(key, RemovalVerdict::Blocked(now + remaining));
                            blocked_keys.push(key);
                            remembered.push(key);
                        } else {
                            self.removal_cache.insert(key, RemovalVerdict::Clear(now));
                        }
                    }
                    for key in misses {
                        self.removal_cache
                            .entry(key)
                            .or_insert(RemovalVerdict::Clear(now));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "removal_cooldowns failed; admitting the batch unchecked"
                    );
                }
            }
            if self.removal_cache.len() > REMOVAL_CACHE_KEYS {
                self.removal_cache.clear();
            }
        }
        if !remembered.is_empty()
            && let Err(e) = self.store.note_removed_sightings(&remembered).await
        {
            tracing::warn!(
                error = %e,
                "note_removed_sightings failed; continuing with the batch"
            );
        }
        if !blocked_keys.is_empty() {
            metrics::counter!(METRIC_BLOCKED, "reason" => "removal_cooldown")
                .increment(u64::try_from(blocked_keys.len()).unwrap_or(u64::MAX));
            for key in blocked_keys {
                self.batch.remove(&key);
            }
        }
    }

    /// Writes the batch, retrying with exponential backoff until it
    /// succeeds, `cancel` fires or `deadline` passes. Keys enter the dedup
    /// set only after a successful write.
    ///
    /// Before writing, the batch passes the removal-memory gate (§4a):
    /// keys still in cooldown are dropped (counted as
    /// `dc3_blocked_total{reason="removal_cooldown"}`) and counted as
    /// post-removal sightings; keys announced with `seed=1` refresh their
    /// scrape clock first (§4b win 6). Both helpers are best-effort: a
    /// failed check admits the key (intake liveness beats duplicate
    /// fetches; the scrape worker re-tombstones what revives wrongly).
    pub async fn flush(
        &mut self,
        cancel: Option<&CancellationToken>,
        deadline: Option<Instant>,
    ) -> Result<ObserveOutcome, FlushStopped> {
        if !self.seed_announced.is_empty() {
            let keys: Vec<DhtKey> = self.seed_announced.iter().copied().collect();
            self.seed_announced.clear();
            if let Err(e) = self.store.refresh_scraped(&keys).await {
                tracing::warn!(
                    error = %e,
                    keys = keys.len(),
                    "refresh_scraped failed; continuing with the batch"
                );
            }
        }
        if self.batch.is_empty() {
            return Ok(ObserveOutcome::default());
        }
        self.gate_removals().await;
        if self.batch.is_empty() {
            return Ok(ObserveOutcome::default());
        }
        let batch = self.observations();
        let mut delay = self.tuning.retry_base;
        loop {
            match self.store.observe(&batch, self.max_pending).await {
                Ok(outcome) => {
                    let now = Instant::now();
                    let mut rotated = false;
                    for o in &batch {
                        rotated |= self.dedup.insert(&o.key, now);
                    }
                    if rotated {
                        self.sources.clear();
                    }
                    self.batch.clear();
                    if outcome.denied > 0 {
                        metrics::counter!(METRIC_BLOCKED, "reason" => "denylisted")
                            .increment(outcome.denied);
                    }
                    if outcome.dropped > 0 {
                        metrics::counter!(METRIC_BLOCKED, "reason" => "queue_full")
                            .increment(outcome.dropped);
                    }
                    return Ok(outcome);
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        keys = batch.len(),
                        retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        "writing discovered keys failed; retrying"
                    );
                }
            }
            let wake = Instant::now() + delay;
            if let Some(deadline) = deadline
                && wake > deadline
            {
                return Err(FlushStopped::Deadline);
            }
            match cancel {
                Some(token) => {
                    tokio::select! {
                        () = token.cancelled() => return Err(FlushStopped::Cancelled),
                        () = tokio::time::sleep_until(wake) => {}
                    }
                }
                None => tokio::time::sleep_until(wake).await,
            }
            delay = delay.saturating_mul(2).min(self.tuning.retry_max);
        }
    }

    /// Runs until `stop` fires or the channel closes, then drains the
    /// channel and writes the last batch (retrying for at most
    /// `shutdown_flush_timeout`).
    pub async fn run<P: PeerSource>(
        mut self,
        mut rx: mpsc::Receiver<Discovered>,
        peers: P,
        stop: CancellationToken,
    ) {
        let mut ticker = tokio::time::interval(self.tuning.flush_interval.max(MIN_FLUSH_INTERVAL));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut own = OwnAddrs::of(&peers);
        let mut own_read_at = Instant::now();
        loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                event = rx.recv() => {
                    let Some(event) = event else { break };
                    let now = Instant::now();
                    if now.saturating_duration_since(own_read_at) >= OWN_ADDRS_REFRESH {
                        own = OwnAddrs::of(&peers);
                        own_read_at = now;
                    }
                    self.handle(event, &own, now);
                    if self.batch.len() >= self.tuning.batch_max_keys {
                        // A cancelled flush keeps its batch for the final flush.
                        let _ = self.flush(Some(&stop), None).await;
                    }
                }
                _ = ticker.tick() => {
                    let _ = self.flush(Some(&stop), None).await;
                }
            }
        }
        let now = Instant::now();
        while let Ok(event) = rx.try_recv() {
            self.handle(event, &own, now);
        }
        let deadline = Instant::now() + self.tuning.shutdown_flush_timeout;
        match self.flush(None, Some(deadline)).await {
            Ok(_) => tracing::debug!("admission stopped"),
            Err(_) => tracing::warn!(
                keys = self.batch.len(),
                "admission stopped without writing its last batch"
            ),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::memstore::MemoryStore;

    fn key(n: u8) -> DhtKey {
        DhtKey([n; 20])
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn event(k: DhtKey, source: Source, from: &str, peer: Option<&str>) -> Discovered {
        Discovered {
            key: k,
            source,
            peer: peer.map(sa),
            seed: false,
            from: ip(from),
        }
    }

    fn seed_event(k: DhtKey) -> Discovered {
        Discovered {
            key: k,
            source: Source::Announce,
            peer: None,
            seed: true,
            from: ip("8.8.8.8"),
        }
    }

    fn stored_torrent(k: DhtKey) -> dc3_store::NewTorrent {
        dc3_store::NewTorrent {
            dht_key: k,
            info_hash_v1: Some(k),
            info_hash_v2: None,
            name: "x".into(),
            total_size: 1,
            file_count: 0,
            files: Vec::new(),
            files_truncated: false,
            piece_length: None,
        }
    }

    fn admission(store: &MemoryStore, filter: PeerFilter) -> Admission<MemoryStore> {
        let tuning = AdmissionTuning::default();
        Admission::new(
            store.clone(),
            i64::MAX,
            filter,
            tuning,
            shared_hints(&tuning),
            Instant::now(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn dedup_generations_rotate() {
        let t0 = Instant::now();
        let mut d = DedupSet::new(3, Duration::from_secs(60), t0);
        assert!(d.is_empty());
        for n in 0..3 {
            assert!(!d.insert(&key(n), t0));
        }
        assert!(d.contains(&key(0)));
        // A full generation rotates on the next new key.
        assert!(d.insert(&key(3), t0));
        assert_eq!(d.len(), (1, 3));
        assert!(d.contains(&key(0)) && d.contains(&key(3)));
        // Re-inserting a present key never rotates.
        assert!(!d.insert(&key(3), t0));
        // Time-based rotation: the oldest generation is forgotten.
        assert!(!d.maybe_rotate(t0 + Duration::from_secs(59)));
        assert!(d.maybe_rotate(t0 + Duration::from_secs(60)));
        assert!(!d.contains(&key(0)));
        assert!(d.contains(&key(3)));
        assert!(d.maybe_rotate(t0 + Duration::from_secs(120)));
        assert!(d.is_empty());
    }

    #[test]
    fn source_networks() {
        assert_eq!(
            source_network(ip("1.2.3.4")),
            source_network(ip("1.2.3.200"))
        );
        assert_ne!(source_network(ip("1.2.3.4")), source_network(ip("1.2.4.4")));
        assert_eq!(
            source_network(ip("::ffff:1.2.3.4")),
            source_network(ip("1.2.3.9"))
        );
        assert_eq!(
            source_network(ip("2001:db8:1:ffff::1")),
            source_network(ip("2001:db8:1::2"))
        );
        assert_ne!(
            source_network(ip("2001:db8:1::1")),
            source_network(ip("2001:db8:2::1"))
        );
        // IPv4 and IPv6 networks never collide.
        assert_ne!(source_network(ip("0.0.0.1")), source_network(ip("::")));
        assert_ne!(
            source_network(ip("0.1.0.0")),
            source_network(ip("0:0:0:0:0:0:0:1"))
        );
        let mut t = SourceTracker::new(2);
        assert_eq!(t.record(key(1), ip("1.2.3.4")), 1);
        assert_eq!(t.record(key(1), ip("1.2.3.5")), 1);
        assert_eq!(t.record(key(1), ip("5.6.7.8")), 2);
        for n in 10..20 {
            t.record(key(1), ip(&format!("{n}.0.0.1")));
        }
        assert_eq!(t.record(key(1), ip("99.0.0.1")), SOURCE_PREFIXES_PER_KEY);
        t.record(key(2), ip("1.1.1.1"));
        t.record(key(3), ip("1.1.1.1"));
        assert_eq!(t.len(), 2, "bounded LRU");
        assert_eq!(t.record(key(1), ip("5.6.7.8")), 1, "key 1 was evicted");
    }

    #[tokio::test(start_paused = true)]
    async fn get_peers_needs_two_networks() {
        let store = MemoryStore::new();
        let mut a = admission(&store, PeerFilter::PRODUCTION);
        let own = OwnAddrs::default();
        let now = Instant::now();
        let k = key(7);
        a.handle(event(k, Source::GetPeers, "1.2.3.4", None), &own, now);
        a.handle(event(k, Source::GetPeers, "1.2.3.99", None), &own, now);
        assert_eq!(a.batch_len(), 0, "one /24 is not enough");
        a.handle(event(k, Source::GetPeers, "9.9.9.9", None), &own, now);
        assert_eq!(a.batch_len(), 1);
        assert!(a.sources().is_empty());
        // More sightings merge into the batch.
        a.handle(event(k, Source::GetPeers, "9.9.9.9", None), &own, now);
        // Samples and announces are admitted at once, with priority.
        a.handle(event(key(8), Source::Sample, "1.2.3.4", None), &own, now);
        a.handle(event(key(9), Source::Announce, "1.2.3.4", None), &own, now);
        assert_eq!(a.batch_len(), 3);
        a.flush(None, None).await.unwrap();
        let observed = store.observed();
        let find = |k: DhtKey| observed.iter().find(|o| o.key == k).copied().unwrap();
        assert_eq!(find(key(7)).sightings, 2);
        assert!(!find(key(7)).priority);
        assert!(find(key(8)).priority && find(key(9)).priority);
        assert_eq!(store.pending_keys().len(), 3);
        // Keys now in the dedup set are skipped.
        a.handle(event(key(8), Source::Sample, "1.2.3.4", None), &own, now);
        assert_eq!(a.batch_len(), 0);
        // IPv6 /48s count as networks too.
        let k6 = key(10);
        a.handle(
            event(k6, Source::GetPeers, "2001:db8:1:2::1", None),
            &own,
            now,
        );
        a.handle(
            event(k6, Source::GetPeers, "2001:db8:1:3::1", None),
            &own,
            now,
        );
        assert_eq!(a.batch_len(), 0);
        a.handle(
            event(k6, Source::GetPeers, "2001:db8:2::1", None),
            &own,
            now,
        );
        assert_eq!(a.batch_len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn source_counts_reset_with_the_generation() {
        let store = MemoryStore::new();
        let mut a = admission(&store, PeerFilter::PRODUCTION);
        let own = OwnAddrs::default();
        let t0 = Instant::now();
        a.handle(event(key(1), Source::GetPeers, "1.2.3.4", None), &own, t0);
        let later = t0 + DEDUP_ROTATION;
        a.handle(
            event(key(1), Source::GetPeers, "5.6.7.8", None),
            &own,
            later,
        );
        assert_eq!(a.batch_len(), 0, "the first sighting was forgotten");
        a.handle(
            event(key(1), Source::GetPeers, "9.9.9.9", None),
            &own,
            later,
        );
        assert_eq!(a.batch_len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn keys_enter_dedup_only_after_a_successful_write() {
        let store = MemoryStore::new();
        let tuning = AdmissionTuning {
            retry_base: Duration::from_millis(100),
            ..AdmissionTuning::default()
        };
        let mut a = Admission::new(
            store.clone(),
            i64::MAX,
            PeerFilter::PRODUCTION,
            tuning,
            shared_hints(&tuning),
            Instant::now(),
        );
        let own = OwnAddrs::default();
        a.handle(
            event(key(1), Source::Sample, "1.2.3.4", None),
            &own,
            Instant::now(),
        );
        // Two failures before the deadline, one when cancelled, and one more
        // before the final write succeeds.
        store.fail_next_observes(4);
        // A deadline shorter than the retries gives up and keeps the batch.
        let deadline = Instant::now() + Duration::from_millis(150);
        assert_eq!(
            a.flush(None, Some(deadline)).await,
            Err(FlushStopped::Deadline)
        );
        assert_eq!(a.batch_len(), 1);
        assert!(!a.dedup().contains(&key(1)));
        // A cancelled flush keeps it too.
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            a.flush(Some(&token), None).await,
            Err(FlushStopped::Cancelled)
        );
        assert!(!a.dedup().contains(&key(1)));
        let before = Instant::now();
        let outcome = a.flush(None, None).await.unwrap();
        assert_eq!(outcome.queued, 1);
        assert!(Instant::now() - before >= Duration::from_millis(100));
        assert!(a.dedup().contains(&key(1)));
        assert_eq!(a.batch_len(), 0);
        assert_eq!(store.observe_calls(), 5);
        // Sightings of a key waiting in a failed batch are merged, not lost.
        store.fail_next_observes(1);
        a.handle(
            event(key(2), Source::Sample, "1.2.3.4", None),
            &own,
            Instant::now(),
        );
        let token = CancellationToken::new();
        token.cancel();
        assert!(a.flush(Some(&token), None).await.is_err());
        a.handle(
            event(key(2), Source::Sample, "1.2.3.4", None),
            &own,
            Instant::now(),
        );
        a.flush(None, None).await.unwrap();
        let last = store
            .observed()
            .into_iter()
            .rfind(|o| o.key == key(2))
            .unwrap();
        assert_eq!(last.sightings, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn hints_are_filtered_and_expire() {
        let store = MemoryStore::new();
        let filter = PeerFilter::PRODUCTION;
        let mut a = admission(&store, filter);
        let own = OwnAddrs::new(vec![ip("8.8.4.4")], vec![]);
        let t0 = Instant::now();
        let k = key(3);
        // Only the first is kept: then ours, private, loopback and port 0.
        for peer in [
            "8.8.8.8:6881",
            "8.8.4.4:6881",
            "10.0.0.1:6881",
            "127.0.0.1:6881",
            "1.1.1.1:0",
        ] {
            a.handle(event(k, Source::Announce, "8.8.8.8", Some(peer)), &own, t0);
        }
        let hints = a.hints.clone();
        assert_eq!(take_hints(&hints, &k, t0), vec![sa("8.8.8.8:6881")]);
        assert!(take_hints(&hints, &k, t0).is_empty(), "taking removes");

        // Expiry, newest first, and at most eight per key.
        let mut map = HintMap::new(10, HINT_TTL);
        map.add(k, sa("1.1.1.1:1"), t0);
        for port in 2..=10u16 {
            map.add(k, SocketAddr::new(ip("1.1.1.1"), port), t0 + HINT_TTL / 2);
        }
        let got = map.take(&k, t0 + HINT_TTL);
        assert_eq!(got.len(), HINT_PEERS_PER_KEY);
        assert_eq!(got[0], sa("1.1.1.1:10"));
        assert!(!got.contains(&sa("1.1.1.1:1")));
        map.add(k, sa("1.1.1.1:1"), t0);
        assert!(map.take(&k, t0 + HINT_TTL).is_empty(), "expired");
        // The map is a bounded LRU.
        let mut small = HintMap::new(2, HINT_TTL);
        for n in 0..5 {
            small.add(key(n), sa("1.1.1.1:1"), t0);
        }
        assert_eq!(small.len(), 2);
        assert!(small.take(&key(0), t0).is_empty());
        assert_eq!(small.take(&key(4), t0).len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn run_flushes_on_interval_and_at_shutdown() {
        #[derive(Clone)]
        struct NoPeers;
        impl PeerSource for NoPeers {
            async fn get_peers(&self, _: DhtKey, _: Duration) -> Vec<SocketAddr> {
                Vec::new()
            }
            async fn scrape_peers(&self, _: DhtKey, _: Duration) -> dc3_dht::ScrapeReport {
                dc3_dht::ScrapeReport::default()
            }
            fn own_ips(&self) -> Vec<IpAddr> {
                Vec::new()
            }
            fn own_endpoints(&self) -> Vec<SocketAddr> {
                Vec::new()
            }
        }

        let store = MemoryStore::new();
        let a = admission(&store, PeerFilter::PRODUCTION);
        let (tx, rx) = mpsc::channel(16);
        let stop = CancellationToken::new();
        let task = tokio::spawn(a.run(rx, NoPeers, stop.clone()));
        tx.send(event(key(1), Source::Sample, "1.2.3.4", None))
            .await
            .unwrap();
        tokio::time::sleep(BATCH_FLUSH_INTERVAL * 2).await;
        assert_eq!(store.pending_keys(), vec![key(1)]);
        // Events still queued at shutdown are written too.
        tx.send(event(key(2), Source::Sample, "1.2.3.4", None))
            .await
            .unwrap();
        stop.cancel();
        task.await.unwrap();
        assert_eq!(store.pending_keys(), vec![key(1), key(2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_skips_keys_in_removal_cooldown() {
        use crate::stores::CrawlStore;

        let store = MemoryStore::new();
        let mut a = admission(&store, PeerFilter::PRODUCTION);
        let own = OwnAddrs::new(vec![ip("8.8.4.4")], vec![]);
        let t0 = Instant::now();
        // Key 1 was scrape-tombstoned, so its removal is remembered.
        let dead = key(1);
        let id = store.complete(&dead, &stored_torrent(dead)).await.unwrap();
        let items = store
            .claim_scrape_due(10, Duration::ZERO, Duration::ZERO)
            .await
            .unwrap();
        let snap = items.iter().find(|c| c.id == id).unwrap().clone();
        assert!(
            store
                .tombstone_dead(id, snap.last_seen_at, snap.change_seq)
                .await
                .unwrap()
        );
        // Key 2 is fresh.
        let live = key(2);
        a.handle(event(dead, Source::Announce, "1.2.3.4", None), &own, t0);
        a.handle(event(live, Source::Announce, "1.2.3.4", None), &own, t0);
        let out = a.flush(None, None).await.unwrap();
        assert_eq!(out.queued, 1);
        assert_eq!(store.pending_keys(), vec![live]);
        // Still blocked on the next flush (cached verdict): no re-fetch
        // storm while the cooldown runs.
        a.handle(event(dead, Source::Announce, "5.6.7.8", None), &own, t0);
        let out = a.flush(None, None).await.unwrap();
        assert_eq!(out.queued, 0);
        assert_eq!(store.pending_keys(), vec![live]);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_refreshes_seed_announced_scrapes() {
        use crate::stores::CrawlStore;

        let store = MemoryStore::new();
        let mut a = admission(&store, PeerFilter::PRODUCTION);
        let own = OwnAddrs::new(vec![ip("8.8.4.4")], vec![]);
        let k = key(5);
        let id = store.complete(&k, &stored_torrent(k)).await.unwrap();
        store.record_scrape(id, Some(3), 1).await.unwrap();
        // A seed announce is also a discovery (its peer is a hint), and it
        // refreshes the scrape clock without a lookup.
        a.handle(seed_event(k), &own, Instant::now());
        a.flush(None, None).await.unwrap();
        let items = store
            .claim_scrape_due(10, Duration::ZERO, Duration::ZERO)
            .await
            .unwrap();
        let item = items.iter().find(|c| c.id == id).unwrap();
        assert_eq!(item.scrape_failures, 0);
        assert_eq!(item.seeders_est, Some(3));
    }
}
