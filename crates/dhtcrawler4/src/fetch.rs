//! Fetch workers (design §13): claim a key, find peers, fetch its metadata
//! over BEP 9, verify and parse it, and store the result.
//!
//! Each worker:
//! 1. claims up to 8 keys with a 120 s lease, renewed in the background
//!    while the batch is worked (a full batch can outlive the lease);
//! 2. collects peers from the hint map and `get_peers` (10 s), each passing
//!    the address chokepoint and the per-destination limits;
//! 3. tries up to 8 peers, 3 at a time, inside the global connection limit
//!    and the metadata byte budget, all within 45 s;
//! 4. verifies and parses the metadata;
//! 5. records successes in one transaction and failures and give-ups in one
//!    each (`complete_batch`, `fail_batch`, `give_up_batch`), falling back
//!    to one key at a time when a batch is rejected.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dc3_core::DhtKey;
use dc3_dht::compact::canonical_addr;
use dc3_peer::{BYTE_BUDGET_UNIT, FetchError, FetchLimits};
use dc3_store::{FileRow, NewTorrent, PendingItem, StoreError};
use dc3_torrent::TorrentMeta;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::admission::{SharedHints, take_hints};
use crate::peers::{OwnAddrs, PeerFilter, PeerSource};
use crate::stores::CrawlStore;

/// Queue items claimed at a time by one worker.
pub const CLAIM_BATCH: i64 = 8;
/// Lease on a claimed key.
pub const CLAIM_LEASE: Duration = Duration::from_secs(120);
/// How often a lease is renewed while its key is being worked on.
pub const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(90);
/// Shortest pause of an idle worker.
pub const IDLE_SLEEP_MIN: Duration = Duration::from_secs(1);
/// Longest pause of an idle worker.
pub const IDLE_SLEEP_MAX: Duration = Duration::from_secs(3);
/// Time allowed for the `get_peers` lookup of one key.
pub const GET_PEERS_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed for all fetch attempts of one key.
pub const KEY_DEADLINE: Duration = Duration::from_secs(45);
/// Peers tried per key.
pub const MAX_PEER_ATTEMPTS: usize = 8;
/// Peers tried at the same time per key.
pub const PARALLEL_ATTEMPTS: usize = 3;
/// TCP connect timeout per peer.
pub const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Handshake timeout per peer.
pub const PEER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(4);
/// Whole-fetch timeout per peer.
pub const PEER_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Concurrent connections to one destination IP.
pub const DEST_MAX_CONCURRENT: u32 = 2;
/// Connection attempts to one destination IP per window.
pub const DEST_MAX_ATTEMPTS: usize = 10;
/// The attempt-counting window.
pub const DEST_WINDOW: Duration = Duration::from_secs(60);
/// How long a refused or timed-out destination is skipped.
pub const DEST_NEGATIVE_TTL: Duration = Duration::from_secs(10 * 60);
/// Destination IPs tracked; new ones are refused when full.
pub const DEST_CAPACITY: usize = 100_000;
/// Least time between two sweeps of a full destination map.
pub const DEST_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
/// First retry delay after a failed claim.
pub const STORE_RETRY_BASE: Duration = Duration::from_secs(1);
/// Longest retry delay after failed claims.
pub const STORE_RETRY_MAX: Duration = Duration::from_secs(30);
/// Shortest lease renewal period accepted (guards against a zero period).
const MIN_RENEW_INTERVAL: Duration = Duration::from_millis(10);

const METRIC_FETCH: &str = "dc3_fetch_total";
const METRIC_DESTINATION_SKIPPED: &str = "dc3_destination_skipped_total";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `now + d`, or `now` if that would overflow (it never does in practice).
fn after(now: Instant, d: Duration) -> Instant {
    now.checked_add(d).unwrap_or(now)
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Timings and limits of the fetch workers. `Default` gives the production
/// values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchTuning {
    pub lease: Duration,
    pub renew_every: Duration,
    pub idle_min: Duration,
    pub idle_max: Duration,
    pub get_peers_timeout: Duration,
    pub key_deadline: Duration,
    pub max_attempts: usize,
    pub parallel_attempts: usize,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    pub peer_timeout: Duration,
    pub store_retry_base: Duration,
    pub store_retry_max: Duration,
    pub destinations: DestLimits,
}

impl Default for FetchTuning {
    fn default() -> Self {
        Self {
            lease: CLAIM_LEASE,
            renew_every: LEASE_RENEW_INTERVAL,
            idle_min: IDLE_SLEEP_MIN,
            idle_max: IDLE_SLEEP_MAX,
            get_peers_timeout: GET_PEERS_TIMEOUT,
            key_deadline: KEY_DEADLINE,
            max_attempts: MAX_PEER_ATTEMPTS,
            parallel_attempts: PARALLEL_ATTEMPTS,
            connect_timeout: PEER_CONNECT_TIMEOUT,
            handshake_timeout: PEER_HANDSHAKE_TIMEOUT,
            peer_timeout: PEER_FETCH_TIMEOUT,
            store_retry_base: STORE_RETRY_BASE,
            store_retry_max: STORE_RETRY_MAX,
            destinations: DestLimits::default(),
        }
    }
}

/// How one key ended (the `outcome` label of `dc3_fetch_total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FetchOutcome {
    /// Stored.
    Ok,
    /// No usable peer was found.
    NoPeers,
    /// Peers were found, but no fetch succeeded in time.
    FetchFailed,
    /// The metadata was verified but could not be parsed.
    ParseError,
    /// A BEP 27 private torrent; the key is given up.
    Private,
    /// The final database operation failed.
    StoreError,
}

impl FetchOutcome {
    /// Every outcome.
    pub const ALL: [FetchOutcome; 6] = [
        FetchOutcome::Ok,
        FetchOutcome::NoPeers,
        FetchOutcome::FetchFailed,
        FetchOutcome::ParseError,
        FetchOutcome::Private,
        FetchOutcome::StoreError,
    ];

    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            FetchOutcome::Ok => "ok",
            FetchOutcome::NoPeers => "no_peers",
            FetchOutcome::FetchFailed => "fetch_failed",
            FetchOutcome::ParseError => "parse_error",
            FetchOutcome::Private => "private",
            FetchOutcome::StoreError => "store_error",
        }
    }
}

// ---------------------------------------------------------------------------
// Destination limiter
// ---------------------------------------------------------------------------

/// Per-destination limits. `Default` gives the production values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestLimits {
    pub max_concurrent: u32,
    pub max_attempts: usize,
    pub window: Duration,
    pub negative_ttl: Duration,
    pub capacity: usize,
    pub sweep_interval: Duration,
    /// TEST ONLY: count each IP:port as its own destination, like
    /// `DhtTuning::limits_by_endpoint`. Production counts per IP.
    pub by_endpoint: bool,
}

impl Default for DestLimits {
    fn default() -> Self {
        Self {
            max_concurrent: DEST_MAX_CONCURRENT,
            max_attempts: DEST_MAX_ATTEMPTS,
            window: DEST_WINDOW,
            negative_ttl: DEST_NEGATIVE_TTL,
            capacity: DEST_CAPACITY,
            sweep_interval: DEST_SWEEP_INTERVAL,
            by_endpoint: false,
        }
    }
}

/// Why a destination was skipped (the `reason` label of
/// `dc3_destination_skipped_total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestDenied {
    /// Already at the concurrent-connection limit.
    Busy,
    /// Already at the attempts-per-minute limit.
    RateLimited,
    /// Recently refused or timed out.
    NegativeCache,
    /// The map is full of active destinations.
    Full,
}

impl DestDenied {
    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            DestDenied::Busy => "busy",
            DestDenied::RateLimited => "rate_limited",
            DestDenied::NegativeCache => "negative_cache",
            DestDenied::Full => "map_full",
        }
    }

    /// Whether the peer deserves another chance later in the same obtain.
    /// `Busy`, `RateLimited` and `Full` are momentary (budget frees up as
    /// attempts finish); a live negative-cache entry stays failed until it
    /// expires, so requeueing it could only spin.
    fn retryable(self) -> bool {
        !matches!(self, DestDenied::NegativeCache)
    }
}

/// Peers the destination limiter refused, held for one retry per progress
/// step within the same obtain instead of being dropped for the key.
///
/// `progress` counts completed work (finished attempts, finished lookups);
/// requeueing only when it advanced guarantees a permanently-refused peer
/// waits for real time to pass instead of spinning, and terminates because
/// progress steps are bounded by the attempt budget plus one lookup.
#[derive(Debug, Default)]
struct Deferred {
    peers: Vec<SocketAddr>,
    requeued_at: usize,
}

impl Deferred {
    fn push(&mut self, peer: SocketAddr) {
        self.peers.push(peer);
    }

    fn requeue(&mut self, queue: &mut VecDeque<SocketAddr>, progress: usize) {
        if !self.peers.is_empty() && progress != self.requeued_at {
            queue.extend(self.peers.drain(..));
            self.requeued_at = progress;
        }
    }
}

#[derive(Debug)]
struct Destination {
    active: u32,
    attempts: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl Destination {
    fn idle(&self, now: Instant, window: Duration) -> bool {
        self.active == 0
            && self.blocked_until.is_none_or(|until| until <= now)
            && self
                .attempts
                .back()
                .is_none_or(|at| now.saturating_duration_since(*at) >= window)
    }
}

#[derive(Debug, Default)]
struct LimiterState {
    /// Keyed by IP (with port 0), or by endpoint with `by_endpoint`.
    map: HashMap<SocketAddr, Destination>,
    last_sweep: Option<Instant>,
}

/// Limits connections per destination IP: concurrency, attempts per
/// minute, and a negative cache. The map is bounded; when it is full of
/// destinations that are still in use, new ones are refused rather than
/// evicting live entries.
#[derive(Debug)]
pub struct DestinationLimiter {
    limits: DestLimits,
    state: Mutex<LimiterState>,
}

impl DestinationLimiter {
    /// A new limiter.
    pub fn new(limits: DestLimits) -> Arc<Self> {
        Arc::new(Self {
            limits,
            state: Mutex::new(LimiterState::default()),
        })
    }

    fn key(&self, peer: SocketAddr) -> SocketAddr {
        let peer = canonical_addr(peer);
        if self.limits.by_endpoint {
            peer
        } else {
            SocketAddr::new(peer.ip(), 0)
        }
    }

    /// Starts an attempt to `peer`, or says why not.
    pub fn try_acquire(
        self: &Arc<Self>,
        peer: SocketAddr,
        now: Instant,
    ) -> Result<DestPermit, DestDenied> {
        let ip = self.key(peer);
        let limits = self.limits;
        let mut state = lock(&self.state);
        if !state.map.contains_key(&ip) && state.map.len() >= limits.capacity {
            let sweep_due = state
                .last_sweep
                .is_none_or(|at| now.saturating_duration_since(at) >= limits.sweep_interval);
            if sweep_due {
                state.map.retain(|_, d| !d.idle(now, limits.window));
                state.last_sweep = Some(now);
            }
            if state.map.len() >= limits.capacity {
                return Err(DestDenied::Full);
            }
        }
        let dest = state.map.entry(ip).or_insert_with(|| Destination {
            active: 0,
            attempts: VecDeque::new(),
            blocked_until: None,
        });
        if let Some(until) = dest.blocked_until {
            if now < until {
                return Err(DestDenied::NegativeCache);
            }
            dest.blocked_until = None;
        }
        while dest
            .attempts
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= limits.window)
        {
            dest.attempts.pop_front();
        }
        if dest.active >= limits.max_concurrent {
            return Err(DestDenied::Busy);
        }
        if dest.attempts.len() >= limits.max_attempts {
            return Err(DestDenied::RateLimited);
        }
        dest.active = dest.active.saturating_add(1);
        dest.attempts.push_back(now);
        Ok(DestPermit {
            limiter: Arc::clone(self),
            ip,
        })
    }

    /// Destinations tracked.
    pub fn len(&self) -> usize {
        lock(&self.state).map.len()
    }

    /// True when no destination is tracked.
    pub fn is_empty(&self) -> bool {
        lock(&self.state).map.is_empty()
    }
}

/// One attempt in progress; dropping it frees the slot.
#[derive(Debug)]
pub struct DestPermit {
    limiter: Arc<DestinationLimiter>,
    ip: SocketAddr,
}

impl DestPermit {
    /// Ends the attempt. A `negative` attempt (refused or timed out) makes
    /// the destination skipped for the negative-cache time.
    pub fn finish(self, negative: bool, now: Instant) {
        if negative {
            let until = after(now, self.limiter.limits.negative_ttl);
            if let Some(dest) = lock(&self.limiter.state).map.get_mut(&self.ip) {
                dest.blocked_until = Some(until);
            }
        }
    }
}

impl Drop for DestPermit {
    fn drop(&mut self) {
        if let Some(dest) = lock(&self.limiter.state).map.get_mut(&self.ip) {
            dest.active = dest.active.saturating_sub(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

/// What fetched metadata turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    /// It does not hash to the key.
    Mismatch,
    /// It could not be parsed.
    Invalid,
    /// A BEP 27 private torrent.
    Private,
    /// A torrent that may be stored.
    Valid(TorrentMeta),
}

/// Verifies `info` against `key` and parses it. CPU-bound; run it off the
/// async threads.
pub fn inspect(key: &DhtKey, info: &[u8]) -> Inspection {
    if dc3_torrent::verify(key, info).is_none() {
        return Inspection::Mismatch;
    }
    let parsed = dc3_torrent::parse_info_visit(info, &mut |_| {});
    match parsed {
        Err(_) => Inspection::Invalid,
        Ok(meta) if meta.private => Inspection::Private,
        Ok(meta) => Inspection::Valid(meta),
    }
}

/// The store record for a parsed torrent.
pub fn new_torrent(key: DhtKey, meta: TorrentMeta) -> NewTorrent {
    NewTorrent {
        dht_key: key,
        info_hash_v1: meta.info_hash_v1,
        info_hash_v2: meta.info_hash_v2,
        name: meta.name,
        total_size: meta.total_size,
        file_count: meta.file_count,
        files: meta
            .files
            .into_iter()
            .map(|f| FileRow {
                path: f.path,
                size: f.size,
            })
            .collect(),
        files_truncated: meta.files_truncated,
        piece_length: meta.piece_length,
    }
}

// ---------------------------------------------------------------------------
// Fetcher
// ---------------------------------------------------------------------------

/// Renews a lease in the background until dropped.
struct RenewalGuard(tokio::task::JoinHandle<()>);

/// True when a background lease renewal could act before the fetch's own
/// deadline: the first tick fires at `renew_every`, and past `key_deadline`
/// the fetch is already timed out. With default tuning (90 s vs 45 s) the
/// tick never fires, so spawning the guard is pure overhead (a task plus a
/// timer per key across all workers).
fn renewal_before_deadline(renew_every: Duration, key_deadline: Duration) -> bool {
    renew_every.max(MIN_RENEW_INTERVAL) < key_deadline
}

impl RenewalGuard {
    fn spawn<S: CrawlStore>(store: S, key: DhtKey, lease: Duration, every: Duration) -> Self {
        let every = every.max(MIN_RENEW_INTERVAL);
        Self(tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(after(Instant::now(), every), every);
            loop {
                ticker.tick().await;
                match store.renew(&key, lease).await {
                    Ok(true) => {}
                    Ok(false) => tracing::debug!("a queue lease was lost during a fetch"),
                    Err(e) => tracing::warn!(error = %e, "renewing a queue lease failed"),
                }
            }
        }))
    }

    /// Renews every key of a claimed batch until dropped. A batch holds its
    /// leases for the whole sequential fetch run plus the batched store: up
    /// to `CLAIM_BATCH` × `key_deadline` (8 × 45 s = 360 s by default), far
    /// past the 120 s lease — even though one key alone (45 s) never needs
    /// renewal. Without this, early keys' leases expire mid-batch and another
    /// worker can claim and re-fetch them (wasted work, double attempts).
    /// Returns `None` when renewal cannot help (no keys, or the first tick
    /// would fire after the lease already expired).
    fn spawn_all<S: CrawlStore>(
        store: S,
        keys: &[DhtKey],
        lease: Duration,
        every: Duration,
    ) -> Option<Self> {
        if keys.is_empty() {
            return None;
        }
        let every = every.max(MIN_RENEW_INTERVAL);
        if every >= lease {
            return None;
        }
        let keys = keys.to_vec();
        Some(Self(tokio::spawn(async move {
            let mut ticker = tokio::time::interval_at(after(Instant::now(), every), every);
            loop {
                ticker.tick().await;
                for key in &keys {
                    match store.renew(key, lease).await {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::debug!("a queue lease was lost during a fetch");
                        }
                        Err(e) => tracing::warn!(error = %e, "renewing a queue lease failed"),
                    }
                }
            }
        })))
    }
}

impl Drop for RenewalGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum Obtained {
    Metadata(Vec<u8>),
    NoPeers,
    Failed,
}

/// Everything the fetch workers share.
pub struct Fetcher<S, P> {
    store: S,
    peers: P,
    filter: PeerFilter,
    hints: SharedHints,
    limiter: Arc<DestinationLimiter>,
    connections: Semaphore,
    byte_budget: Arc<Semaphore>,
    max_metadata: usize,
    tuning: FetchTuning,
}

impl<S, P> std::fmt::Debug for Fetcher<S, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher")
            .field("tuning", &self.tuning)
            .finish_non_exhaustive()
    }
}

/// Sizes of the shared fetch resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchLimitsConfig {
    /// Concurrent peer connections in total (`crawl.max_connections`).
    pub max_connections: usize,
    /// Largest accepted `metadata_size` (`crawl.max_metadata_bytes`).
    pub max_metadata_bytes: usize,
    /// Metadata bytes held by all fetches (`crawl.max_inflight_metadata_bytes`).
    pub max_inflight_metadata_bytes: usize,
}

/// What one claimed key needs from the database, decided without any store
/// access.
#[derive(Debug)]
enum Staged {
    /// No usable peer was found.
    Missing,
    /// Peers were found, but no fetch succeeded in time.
    Failed,
    /// Verified metadata that can never be stored, with its outcome.
    Doomed(FetchOutcome),
    /// A torrent ready to store.
    Ready(NewTorrent),
}

impl<S: CrawlStore, P: PeerSource> Fetcher<S, P> {
    /// Shared state for the workers.
    pub fn new(
        store: S,
        peers: P,
        filter: PeerFilter,
        hints: SharedHints,
        limits: FetchLimitsConfig,
        tuning: FetchTuning,
    ) -> Self {
        let budget_permits = (limits.max_inflight_metadata_bytes / BYTE_BUDGET_UNIT)
            .clamp(1, Semaphore::MAX_PERMITS);
        // The test switch applies to every per-address rule.
        let destinations = DestLimits {
            by_endpoint: tuning.destinations.by_endpoint || filter.by_endpoint,
            ..tuning.destinations
        };
        Self {
            store,
            peers,
            filter,
            hints,
            limiter: DestinationLimiter::new(destinations),
            connections: Semaphore::new(limits.max_connections.clamp(1, Semaphore::MAX_PERMITS)),
            byte_budget: Arc::new(Semaphore::new(budget_permits)),
            max_metadata: limits.max_metadata_bytes,
            tuning,
        }
    }

    /// The destination limiter.
    pub fn limiter(&self) -> &Arc<DestinationLimiter> {
        &self.limiter
    }

    /// Fetches, verifies and parses one claimed key: the network half of
    /// [`Fetcher::process`], with no store access and no metrics.
    async fn fetch_item(&self, item: &PendingItem) -> Staged {
        self.fetch_item_inner(item, true).await
    }

    /// [`Fetcher::fetch_item`], with the per-key renewal guard optional.
    /// [`Fetcher::process_batch`] disables it: the batch guard (one task for
    /// all keys, held for the whole batch) covers every key, so per-key
    /// tasks would only duplicate renewals.
    async fn fetch_item_inner(&self, item: &PendingItem, renew: bool) -> Staged {
        let key = item.dht_key;
        let renewal = (renew
            && renewal_before_deadline(self.tuning.renew_every, self.tuning.key_deadline))
        .then(|| {
            RenewalGuard::spawn(
                self.store.clone(),
                key,
                self.tuning.lease,
                self.tuning.renew_every,
            )
        });
        let obtained = tokio::time::timeout(self.tuning.key_deadline, self.obtain(key))
            .await
            .unwrap_or(Obtained::Failed);
        drop(renewal);
        match obtained {
            Obtained::NoPeers => Staged::Missing,
            Obtained::Failed => Staged::Failed,
            Obtained::Metadata(info) => {
                let key = item.dht_key;
                let inspection = tokio::task::spawn_blocking(move || inspect(&key, &info))
                    .await
                    .unwrap_or(Inspection::Invalid);
                match inspection {
                    Inspection::Mismatch => Staged::Failed,
                    Inspection::Invalid => Staged::Doomed(FetchOutcome::ParseError),
                    Inspection::Private => Staged::Doomed(FetchOutcome::Private),
                    Inspection::Valid(meta) => Staged::Ready(new_torrent(key, meta)),
                }
            }
        }
    }

    /// Records one fetched item.
    async fn store_one(&self, item: &PendingItem, staged: Staged) -> FetchOutcome {
        let outcome = match staged {
            Staged::Missing => self.give_back(item.dht_key, FetchOutcome::NoPeers).await,
            Staged::Failed => {
                self.give_back(item.dht_key, FetchOutcome::FetchFailed)
                    .await
            }
            Staged::Doomed(outcome) => self.give_up(item.dht_key, outcome).await,
            Staged::Ready(t) => self.store_ready(item.dht_key, t).await,
        };
        self.emit(item, outcome);
        outcome
    }

    /// The outcome counter and debug log, identical for every store path.
    fn emit(&self, item: &PendingItem, outcome: FetchOutcome) {
        metrics::counter!(METRIC_FETCH, "outcome" => outcome.as_str()).increment(1);
        tracing::debug!(
            outcome = outcome.as_str(),
            earlier_attempts = item.attempts,
            "fetch finished"
        );
    }

    /// Fetches, checks and stores one claimed key.
    pub async fn process(&self, item: &PendingItem) -> FetchOutcome {
        let staged = self.fetch_item(item).await;
        self.store_one(item, staged).await
    }

    async fn obtain(&self, key: DhtKey) -> Obtained {
        let mut own = OwnAddrs::of(&self.peers);
        let mut queue: VecDeque<SocketAddr> = VecDeque::new();
        let mut seen: HashSet<SocketAddr> = HashSet::new();
        for peer in take_hints(&self.hints, &key, Instant::now()) {
            if let Some(peer) = self.filter.accept(peer, &own)
                && seen.insert(peer)
            {
                queue.push_back(peer);
            }
        }
        let lookup = self.peers.scrape_peers(key, self.tuning.get_peers_timeout);
        tokio::pin!(lookup);
        let mut lookup_done = false;
        let mut attempts = 0usize;
        let mut running = FuturesUnordered::new();
        let mut deferred = Deferred::default();
        let mut progress = 0usize;
        loop {
            if queue.is_empty() {
                deferred.requeue(&mut queue, progress);
            }
            while running.len() < self.tuning.parallel_attempts
                && attempts < self.tuning.max_attempts
            {
                let Some(peer) = queue.pop_front() else { break };
                match self.limiter.try_acquire(peer, Instant::now()) {
                    Ok(permit) => {
                        attempts = attempts.saturating_add(1);
                        running.push(self.attempt(peer, key, permit));
                    }
                    Err(denied) => {
                        metrics::counter!(METRIC_DESTINATION_SKIPPED, "reason" => denied.as_str())
                            .increment(1);
                        if denied.retryable() {
                            deferred.push(peer);
                        }
                    }
                }
            }
            if running.is_empty() && (lookup_done || attempts >= self.tuning.max_attempts) {
                break;
            }
            tokio::select! {
                report = &mut lookup, if !lookup_done => {
                    lookup_done = true;
                    progress = progress.saturating_add(1);
                    own = OwnAddrs::of(&self.peers);
                    // §8 piggyback: the traversal was already paid for, so
                    // its estimate is free liveness data for the queue's
                    // ordering. Unaware (None) leaves NULL, never 0.
                    // Best-effort: a lost write only costs ordering.
                    if let Some(est) = report.seeders_est()
                        && let Ok(est) = u32::try_from(est)
                        && let Err(e) = self.store.note_fetch_estimate(&key, est).await
                    {
                        tracing::debug!(error = %e, "note_fetch_estimate failed");
                    }
                    for peer in report.peers {
                        if let Some(peer) = self.filter.accept(peer, &own)
                            && seen.insert(peer)
                        {
                            queue.push_back(peer);
                        }
                    }
                }
                Some(result) = running.next(), if !running.is_empty() => {
                    progress = progress.saturating_add(1);
                    match result {
                        Ok(info) => return Obtained::Metadata(info),
                        Err(e) => tracing::debug!(reason = e.label(), "peer fetch failed"),
                    }
                }
                // Unreachable: the checks above keep one branch enabled.
                else => break,
            }
        }
        if seen.is_empty() {
            Obtained::NoPeers
        } else {
            Obtained::Failed
        }
    }

    async fn attempt(
        &self,
        peer: SocketAddr,
        key: DhtKey,
        permit: DestPermit,
    ) -> Result<Vec<u8>, FetchError> {
        let Ok(_slot) = self.connections.acquire().await else {
            return Err(FetchError::BudgetClosed);
        };
        let limits = FetchLimits {
            connect: self.tuning.connect_timeout,
            handshake: self.tuning.handshake_timeout,
            total: self.tuning.peer_timeout,
            max_metadata: self.max_metadata,
            byte_budget: Some(Arc::clone(&self.byte_budget)),
        };
        let result = dc3_peer::fetch_metadata(peer, key, &limits).await;
        let negative = matches!(result, Err(FetchError::Connect(_) | FetchError::Timeout));
        permit.finish(negative, Instant::now());
        result
    }

    /// Stores one parsed torrent: the transaction half of the old
    /// `store_metadata`.
    async fn store_ready(&self, key: DhtKey, t: NewTorrent) -> FetchOutcome {
        match self.store.complete(&key, &t).await {
            Ok(_) => FetchOutcome::Ok,
            // The store can never accept this torrent (e.g. a size
            // above i64::MAX); retrying would loop on it forever.
            Err(e @ StoreError::Invalid(_)) => {
                tracing::warn!(error = %e, "fetched torrent cannot be stored; giving up");
                self.give_up(key, FetchOutcome::StoreError).await
            }
            Err(e) => {
                tracing::warn!(error = %e, "storing a fetched torrent failed");
                self.give_back(key, FetchOutcome::StoreError).await
            }
        }
    }

    async fn give_back(&self, key: DhtKey, outcome: FetchOutcome) -> FetchOutcome {
        match self.store.fail(&key).await {
            Ok(_) => outcome,
            Err(e) => {
                tracing::warn!(error = %e, "recording a failed fetch failed");
                FetchOutcome::StoreError
            }
        }
    }

    async fn give_up(&self, key: DhtKey, outcome: FetchOutcome) -> FetchOutcome {
        match self.store.give_up(&key).await {
            Ok(_) => outcome,
            Err(e) => {
                tracing::warn!(error = %e, "recording a failed fetch failed");
                FetchOutcome::StoreError
            }
        }
    }

    /// One worker: claims and processes keys until `stop` fires. A key
    /// already claimed is finished first.
    pub async fn run_worker(self: Arc<Self>, stop: CancellationToken) {
        let base = self.tuning.store_retry_base;
        let mut backoff = base;
        while !stop.is_cancelled() {
            let pause = match self.store.claim(CLAIM_BATCH, self.tuning.lease).await {
                Ok(items) if items.is_empty() => {
                    backoff = base;
                    self.idle_pause()
                }
                Ok(items) => {
                    backoff = base;
                    self.process_batch(&items).await;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "claiming queue items failed");
                    let pause = backoff;
                    backoff = backoff.saturating_mul(2).min(self.tuning.store_retry_max);
                    pause
                }
            };
            tokio::select! {
                () = stop.cancelled() => break,
                () = tokio::time::sleep(pause) => {}
            }
        }
    }

    /// Fetches a claimed batch, then records it in as few transactions as
    /// the store allows. Outcomes and metrics match one-by-one `process()`.
    /// One renewal task covers all keys for the whole batch (sequential
    /// fetches plus the batched store can outlive the lease).
    async fn process_batch(&self, items: &[PendingItem]) {
        let keys: Vec<DhtKey> = items.iter().map(|item| item.dht_key).collect();
        let _batch_renewals = RenewalGuard::spawn_all(
            self.store.clone(),
            &keys,
            self.tuning.lease,
            self.tuning.renew_every,
        );
        let mut staged = Vec::with_capacity(items.len());
        for item in items {
            staged.push(self.fetch_item_inner(item, false).await);
        }
        let mut readies: Vec<(&PendingItem, NewTorrent)> = Vec::new();
        let mut faileds: Vec<(&PendingItem, FetchOutcome)> = Vec::new();
        let mut doomeds: Vec<(&PendingItem, FetchOutcome)> = Vec::new();
        for (item, stage) in items.iter().zip(staged) {
            match stage {
                Staged::Ready(t) => readies.push((item, t)),
                Staged::Missing => faileds.push((item, FetchOutcome::NoPeers)),
                Staged::Failed => faileds.push((item, FetchOutcome::FetchFailed)),
                Staged::Doomed(outcome) => doomeds.push((item, outcome)),
            }
        }
        // Successes share one transaction; a rejected batch falls back to
        // one key at a time, exactly as `process()` would.
        if !readies.is_empty() {
            let pairs: Vec<(DhtKey, NewTorrent)> = readies
                .iter()
                .map(|(item, t)| (item.dht_key, t.clone()))
                .collect();
            match self.store.complete_batch(&pairs).await {
                Ok(ids) => {
                    debug_assert_eq!(ids.len(), readies.len());
                    for (item, _) in &readies {
                        self.emit(item, FetchOutcome::Ok);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "batched completion failed; storing one key at a time");
                    for (item, t) in readies {
                        let outcome = self.store_ready(item.dht_key, t).await;
                        self.emit(item, outcome);
                    }
                }
            }
        }
        if !faileds.is_empty() {
            let keys: Vec<DhtKey> = faileds.iter().map(|(item, _)| item.dht_key).collect();
            match self.store.fail_batch(&keys).await {
                Ok(()) => {
                    for (item, outcome) in &faileds {
                        self.emit(item, *outcome);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "batched failure recording failed; recording one key at a time");
                    for (item, outcome) in faileds {
                        let stored = self.give_back(item.dht_key, outcome).await;
                        self.emit(item, stored);
                    }
                }
            }
        }
        if !doomeds.is_empty() {
            let keys: Vec<DhtKey> = doomeds.iter().map(|(item, _)| item.dht_key).collect();
            match self.store.give_up_batch(&keys).await {
                Ok(()) => {
                    for (item, outcome) in &doomeds {
                        self.emit(item, *outcome);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "batched give-up failed; recording one key at a time");
                    for (item, outcome) in doomeds {
                        let stored = self.give_up(item.dht_key, outcome).await;
                        self.emit(item, stored);
                    }
                }
            }
        }
    }

    fn idle_pause(&self) -> Duration {
        let a = millis(self.tuning.idle_min);
        let b = millis(self.tuning.idle_max);
        Duration::from_millis(rand::random_range(a.min(b)..=a.max(b)))
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
    use std::collections::BTreeMap;

    use dc3_bencode::OwnedValue;
    use tokio::net::TcpListener;

    use std::net::IpAddr;

    use super::*;
    use crate::admission::{AdmissionTuning, shared_hints};
    use crate::memstore::MemoryStore;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// A destination on port 6881.
    fn dest(s: &str) -> SocketAddr {
        SocketAddr::new(ip(s), 6881)
    }

    fn limits(capacity: usize) -> DestLimits {
        DestLimits {
            capacity,
            ..DestLimits::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn destination_concurrency() {
        let l = DestinationLimiter::new(limits(10));
        let now = Instant::now();
        let a = l.try_acquire(dest("1.1.1.1"), now).unwrap();
        let b = l.try_acquire(dest("::ffff:1.1.1.1"), now).unwrap();
        assert_eq!(
            l.try_acquire(dest("1.1.1.1"), now).unwrap_err(),
            DestDenied::Busy
        );
        // Another destination is independent.
        let _c = l.try_acquire(dest("1.1.1.2"), now).unwrap();
        drop(a);
        let _d = l.try_acquire(dest("1.1.1.1"), now).unwrap();
        b.finish(false, now);
        let _e = l.try_acquire(dest("1.1.1.1"), now).unwrap();
        assert_eq!(l.len(), 2);

        // In production any port of one IP is the same destination.
        let at = |port| SocketAddr::new(ip("5.5.5.5"), port);
        let _x = l.try_acquire(at(1), now).unwrap();
        let _y = l.try_acquire(at(2), now).unwrap();
        assert_eq!(l.try_acquire(at(3), now).unwrap_err(), DestDenied::Busy);
        // With the test switch each endpoint is its own destination.
        let t = DestinationLimiter::new(DestLimits {
            by_endpoint: true,
            ..limits(10)
        });
        let held: Vec<DestPermit> = (1..=3)
            .map(|port| t.try_acquire(at(port), now).unwrap())
            .collect();
        assert_eq!(held.len(), 3);
        assert_eq!(t.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn destination_attempts_per_minute() {
        let l = DestinationLimiter::new(limits(10));
        let t0 = Instant::now();
        for i in 0..DEST_MAX_ATTEMPTS {
            let at = t0 + Duration::from_secs(u64::try_from(i).unwrap());
            l.try_acquire(dest("2.2.2.2"), at)
                .unwrap()
                .finish(false, at);
        }
        let t = t0 + Duration::from_secs(30);
        assert_eq!(
            l.try_acquire(dest("2.2.2.2"), t).unwrap_err(),
            DestDenied::RateLimited
        );
        // The first attempt leaves the window after a minute.
        let t = t0 + DEST_WINDOW;
        l.try_acquire(dest("2.2.2.2"), t).unwrap();
        assert_eq!(
            l.try_acquire(dest("2.2.2.2"), t).unwrap_err(),
            DestDenied::RateLimited
        );
    }

    #[tokio::test(start_paused = true)]
    async fn destination_negative_cache() {
        let l = DestinationLimiter::new(limits(10));
        let t0 = Instant::now();
        l.try_acquire(dest("3.3.3.3"), t0).unwrap().finish(true, t0);
        let t = t0 + DEST_NEGATIVE_TTL - Duration::from_secs(1);
        assert_eq!(
            l.try_acquire(dest("3.3.3.3"), t).unwrap_err(),
            DestDenied::NegativeCache
        );
        l.try_acquire(dest("3.3.3.3"), t0 + DEST_NEGATIVE_TTL)
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn full_destination_map_refuses_instead_of_evicting() {
        let l = DestinationLimiter::new(limits(2));
        let t0 = Instant::now();
        let held = l.try_acquire(dest("4.4.4.1"), t0).unwrap();
        l.try_acquire(dest("4.4.4.2"), t0).unwrap().finish(true, t0);
        assert_eq!(
            l.try_acquire(dest("4.4.4.3"), t0).unwrap_err(),
            DestDenied::Full
        );
        // Known destinations are still served.
        assert!(l.try_acquire(dest("4.4.4.1"), t0).is_ok());
        // Once entries go idle, a sweep makes room; the one in use stays.
        let t = t0 + DEST_NEGATIVE_TTL;
        l.try_acquire(dest("4.4.4.3"), t).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(
            l.try_acquire(dest("4.4.4.1"), t).map(|_| ()),
            Ok(()),
            "an active destination is never evicted"
        );
        drop(held);
    }

    #[test]
    fn outcome_labels() {
        let labels: Vec<&str> = FetchOutcome::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            labels,
            [
                "ok",
                "no_peers",
                "fetch_failed",
                "parse_error",
                "private",
                "store_error"
            ]
        );
    }

    #[test]
    fn only_momentary_denials_are_retried() {
        assert!(DestDenied::Busy.retryable());
        assert!(DestDenied::RateLimited.retryable());
        assert!(DestDenied::Full.retryable());
        assert!(!DestDenied::NegativeCache.retryable());
    }

    #[test]
    fn deferred_peers_retry_once_per_progress_step() {
        let mut queue = VecDeque::from([dest("9.9.9.9")]);
        let mut deferred = Deferred::default();
        deferred.push(queue.pop_front().unwrap());
        assert!(queue.is_empty());
        // No progress yet: no requeue, so a still-busy peer cannot spin.
        deferred.requeue(&mut queue, 0);
        assert!(queue.is_empty());
        // After other work finished, the peer gets another chance.
        deferred.requeue(&mut queue, 1);
        assert_eq!(queue.len(), 1);
        // ...but only one chance per progress step: a repeat denial waits.
        deferred.push(queue.pop_front().unwrap());
        deferred.requeue(&mut queue, 1);
        assert!(queue.is_empty());
        deferred.requeue(&mut queue, 2);
        assert_eq!(queue.len(), 1);
    }

    /// SHA-1 (FIPS 180-4), to key test data that does not parse.
    fn sha1(data: &[u8]) -> [u8; 20] {
        let mut h: [u32; 5] = [
            0x6745_2301,
            0xEFCD_AB89,
            0x98BA_DCFE,
            0x1032_5476,
            0xC3D2_E1F0,
        ];
        let bit_len = u64::try_from(data.len()).unwrap().wrapping_mul(8);
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());
        for chunk in msg.chunks(64) {
            let mut w = [0u32; 80];
            for i in 0..16 {
                w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let [mut a, mut b, mut c, mut d, mut e] = h;
            for (i, wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                    20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                    _ => (b ^ c ^ d, 0xCA62_C1D6),
                };
                let t = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(*wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = t;
            }
            for (hv, v) in h.iter_mut().zip([a, b, c, d, e]) {
                *hv = hv.wrapping_add(v);
            }
        }
        let mut out = [0u8; 20];
        for (i, v) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }

    /// A v1 multi-file info dictionary whose files are 1 000 bytes each.
    pub(crate) fn info_dict(name: &str, files: &[&str], private: bool) -> Vec<u8> {
        let sized: Vec<(&str, i64)> = files.iter().map(|f| (*f, 1000)).collect();
        info_dict_sized(name, &sized, private)
    }

    /// A v1 multi-file info dictionary with the given file sizes.
    fn info_dict_sized(name: &str, files: &[(&str, i64)], private: bool) -> Vec<u8> {
        let mut d = BTreeMap::new();
        d.insert(b"name".to_vec(), OwnedValue::from(name));
        d.insert(b"piece length".to_vec(), OwnedValue::Int(16384));
        d.insert(b"pieces".to_vec(), OwnedValue::Bytes(vec![7u8; 20]));
        let list = files
            .iter()
            .map(|(path, length)| {
                let mut f = BTreeMap::new();
                f.insert(b"length".to_vec(), OwnedValue::Int(*length));
                f.insert(
                    b"path".to_vec(),
                    OwnedValue::List(path.split('/').map(OwnedValue::from).collect()),
                );
                OwnedValue::Dict(f)
            })
            .collect();
        d.insert(b"files".to_vec(), OwnedValue::List(list));
        if private {
            d.insert(b"private".to_vec(), OwnedValue::Int(1));
        }
        dc3_bencode::encode(&OwnedValue::Dict(d))
    }

    fn key_of(info: &[u8]) -> DhtKey {
        dc3_torrent::parse_info(info).unwrap().info_hash_v1.unwrap()
    }

    #[test]
    fn inspection() {
        let good = info_dict("ubuntu images", &["a/one.iso", "b/two.iso"], false);
        let k = key_of(&good);
        match inspect(&k, &good) {
            Inspection::Valid(meta) => {
                assert_eq!(meta.name, "ubuntu images");
                let t = new_torrent(k, meta);
                assert_eq!(t.files.len(), 2);
                assert_eq!(t.total_size, 2000);
                assert_eq!(t.info_hash_v1, Some(k));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(inspect(&DhtKey([0; 20]), &good), Inspection::Mismatch);
        let private = info_dict("tracker only", &["x.txt"], true);
        assert_eq!(inspect(&key_of(&private), &private), Inspection::Private);
        // The test SHA-1 agrees with dc3-torrent.
        assert_eq!(DhtKey(sha1(&good)), k);
        assert_eq!(
            DhtKey(sha1(b"abc")).to_hex(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // Verified but unparseable: not a torrent, or a negative length.
        let junk = dc3_bencode::encode(&OwnedValue::Dict(BTreeMap::new()));
        assert_eq!(inspect(&DhtKey(sha1(&junk)), &junk), Inspection::Invalid);
        let negative = info_dict_sized("clean name", &[("a.txt", 5), ("b.txt", -1)], false);
        assert_eq!(
            inspect(&DhtKey(sha1(&negative)), &negative),
            Inspection::Invalid
        );
    }

    #[derive(Clone)]
    struct FixedPeers(Vec<SocketAddr>);

    impl PeerSource for FixedPeers {
        async fn get_peers(&self, _: DhtKey, _: Duration) -> Vec<SocketAddr> {
            self.0.clone()
        }
        async fn scrape_peers(&self, _: DhtKey, _: Duration) -> dc3_dht::ScrapeReport {
            dc3_dht::ScrapeReport {
                peers: self.0.clone(),
                ..dc3_dht::ScrapeReport::default()
            }
        }
        fn own_ips(&self) -> Vec<IpAddr> {
            vec![ip("127.0.0.1")]
        }
        fn own_endpoints(&self) -> Vec<SocketAddr> {
            Vec::new()
        }
    }

    fn test_tuning() -> FetchTuning {
        FetchTuning {
            connect_timeout: Duration::from_secs(1),
            handshake_timeout: Duration::from_secs(1),
            peer_timeout: Duration::from_secs(3),
            key_deadline: Duration::from_secs(10),
            idle_min: Duration::from_millis(20),
            idle_max: Duration::from_millis(40),
            ..FetchTuning::default()
        }
    }

    fn fetcher<P: PeerSource>(store: &MemoryStore, peers: P) -> Arc<Fetcher<MemoryStore, P>> {
        fetcher_with(store, peers, test_tuning())
    }

    fn fetcher_with<P: PeerSource>(
        store: &MemoryStore,
        peers: P,
        tuning: FetchTuning,
    ) -> Arc<Fetcher<MemoryStore, P>> {
        let filter = PeerFilter {
            allow_private: true,
            by_endpoint: true,
        };
        Arc::new(Fetcher::new(
            store.clone(),
            peers,
            filter,
            shared_hints(&AdmissionTuning::default()),
            FetchLimitsConfig {
                max_connections: 8,
                max_metadata_bytes: 1024 * 1024,
                max_inflight_metadata_bytes: 1024 * 1024,
            },
            tuning,
        ))
    }

    async fn seeder(info: &[u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(dc3_peer::seeder::serve(
            listener,
            key_of(info),
            info.to_vec(),
        ));
        addr
    }

    fn item(key: DhtKey) -> PendingItem {
        PendingItem {
            dht_key: key,
            attempts: 0,
            seen_count: 1,
        }
    }

    #[test]
    fn renewal_guard_spawns_only_before_the_deadline() {
        // Production tuning: the first tick (90 s) never fires before the
        // fetch times out (45 s).
        assert!(!renewal_before_deadline(LEASE_RENEW_INTERVAL, KEY_DEADLINE));
        assert!(!renewal_before_deadline(
            Duration::from_secs(90),
            Duration::from_secs(60)
        ));
        assert!(renewal_before_deadline(
            Duration::from_millis(50),
            Duration::from_secs(10)
        ));
        // A tick exactly at the deadline races the timeout: useless too.
        assert!(!renewal_before_deadline(
            Duration::from_secs(10),
            Duration::from_secs(10)
        ));
        // A zero interval is clamped to MIN_RENEW_INTERVAL, still useful.
        assert!(renewal_before_deadline(
            Duration::ZERO,
            Duration::from_secs(10)
        ));
    }

    #[tokio::test]
    async fn batch_renewals_spawn_only_when_useful() {
        let store = MemoryStore::new();
        let one = [DhtKey([1; 20])];
        // Production tuning: a full batch (8 x 45 s) far outlives the lease.
        assert!(
            RenewalGuard::spawn_all(store.clone(), &one, CLAIM_LEASE, LEASE_RENEW_INTERVAL)
                .is_some()
        );
        // Nothing to renew, or the first tick would fire after expiry.
        assert!(
            RenewalGuard::spawn_all(store.clone(), &[], CLAIM_LEASE, LEASE_RENEW_INTERVAL)
                .is_none()
        );
        assert!(
            RenewalGuard::spawn_all(
                store.clone(),
                &one,
                Duration::from_secs(120),
                Duration::from_secs(120)
            )
            .is_none()
        );
        assert!(
            RenewalGuard::spawn_all(
                store.clone(),
                &one,
                Duration::from_secs(120),
                Duration::from_secs(300)
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn batch_renewals_cover_slow_batches() {
        // Per-key renewal is off here (the first tick would race the key
        // deadline), so any renewal must come from the batch guard.
        let tuning = FetchTuning {
            key_deadline: Duration::from_millis(200),
            renew_every: Duration::from_millis(200),
            ..test_tuning()
        };
        assert!(!renewal_before_deadline(
            tuning.renew_every,
            tuning.key_deadline
        ));
        let store = MemoryStore::new();
        let slow = holding_listener().await;
        let keys = [DhtKey([11; 20]), DhtKey([12; 20])];
        for k in &keys {
            store.enqueue(*k);
        }
        // Leases must exist for renewals to count.
        let claimed = store.claim(2, Duration::from_secs(120)).await.unwrap();
        assert_eq!(claimed.len(), 2);
        let f = fetcher_with(&store, FixedPeers(vec![slow]), tuning);
        let items: Vec<PendingItem> = keys.iter().map(|k| item(*k)).collect();
        // Each fetch runs the 200 ms key deadline; the batch guard ticks at
        // 200 ms and renews both leases mid-batch.
        f.process_batch(&items).await;
        assert!(
            store.renewals() >= 1,
            "the batch guard should have renewed, got {}",
            store.renewals()
        );
        for k in &keys {
            assert_eq!(store.failures(k), 1);
        }

        // Contrast: one key at a time spawns no guard under this tuning.
        let before = store.renewals();
        let k = DhtKey([13; 20]);
        store.enqueue(k);
        let claimed = store.claim(1, Duration::from_secs(120)).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(f.process(&item(k)).await, FetchOutcome::FetchFailed);
        assert_eq!(store.renewals(), before);
    }

    #[tokio::test]
    async fn process_batch_falls_back_to_one_key_at_a_time() {
        let first = info_dict("fallback one", &["a.txt"], false);
        let second = info_dict("fallback two", &["b.txt"], false);
        let good = seeder(&first).await;
        let good2 = seeder(&second).await;
        let store = MemoryStore::new();
        let k1 = key_of(&first);
        let k2 = key_of(&second);
        let missing = DhtKey([77; 20]);
        for k in [k1, k2, missing] {
            store.enqueue(k);
        }
        // Both batched writes fail atomically; the per-key fallbacks still
        // succeed, with the same outcomes as one-by-one process().
        store.fail_next_batches(2);
        let f = fetcher(&store, FixedPeers(vec![good, good2]));
        let items: Vec<PendingItem> = [k1, k2, missing].iter().map(|k| item(*k)).collect();
        f.process_batch(&items).await;
        assert_eq!(store.torrent(&k1).unwrap().name, "fallback one");
        assert_eq!(store.torrent(&k2).unwrap().name, "fallback two");
        assert_eq!(store.failures(&missing), 1);
        // The failed key stays queued for retry; the rest are done.
        assert_eq!(store.pending_keys(), vec![missing]);
    }

    /// A peer that accepts connections and then never speaks, so the
    /// handshake runs its full timeout.
    async fn holding_listener() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    drop(sock);
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn slow_fetch_renews_its_lease() {
        let store = MemoryStore::new();
        let k = DhtKey([7; 20]);
        store.enqueue(k);
        // Renewals only count against a claimed lease.
        let claimed = store.claim(1, Duration::from_secs(120)).await.unwrap();
        assert_eq!(claimed.len(), 1);
        let tuning = FetchTuning {
            renew_every: Duration::from_millis(50),
            ..test_tuning()
        };
        let f = fetcher_with(&store, FixedPeers(vec![holding_listener().await]), tuning);
        // The handshake runs its 1 s timeout, far past the 50 ms renewal
        // interval, so the guard must have been spawned and must have
        // renewed the still-queued key.
        assert_eq!(f.process(&claimed[0]).await, FetchOutcome::FetchFailed);
        assert!(store.renewals() >= 1);
    }

    #[tokio::test]
    async fn process_stores_and_fails() {
        let first = info_dict("fetch test", &["a.txt", "b/c.txt"], false);
        let second = info_dict("fetch test second", &["x.txt"], false);
        let private = info_dict("private test", &["a.txt"], true);
        let seeders = [
            seeder(&first).await,
            seeder(&second).await,
            seeder(&private).await,
        ];
        let store = MemoryStore::new();
        for (info, addr, expected) in [
            (&first, seeders[0], FetchOutcome::Ok),
            (&second, seeders[1], FetchOutcome::Ok),
            (&private, seeders[2], FetchOutcome::Private),
        ] {
            let k = key_of(info);
            store.enqueue(k);
            // An unreachable peer first; the right one is still found.
            let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let dead_addr = dead.local_addr().unwrap();
            drop(dead);
            let f = fetcher(&store, FixedPeers(vec![dead_addr, addr]));
            assert_eq!(f.process(&item(k)).await, expected);
        }
        let first_key = key_of(&first);
        let stored = store.torrent(&first_key).unwrap();
        assert_eq!(stored.name, "fetch test");
        assert_eq!(stored.files.len(), 2);
        assert!(store.torrent(&key_of(&second)).is_some());
        // A private torrent is given up at once: not stored, never claimed.
        assert!(store.torrent(&key_of(&private)).is_none());
        assert!(store.pending_keys().is_empty());

        // No peers at all.
        let k = DhtKey([9; 20]);
        store.enqueue(k);
        let f = fetcher(&store, FixedPeers(Vec::new()));
        assert_eq!(f.process(&item(k)).await, FetchOutcome::NoPeers);
        assert_eq!(store.failures(&k), 1);

        // Only our own address and non-dialable ones: no peers either.
        let own = "127.0.0.1:1".parse().unwrap();
        let zero = "0.0.0.0:6881".parse().unwrap();
        let f = Arc::new(Fetcher::new(
            store.clone(),
            FixedPeers(vec![own, zero]),
            PeerFilter {
                allow_private: true,
                by_endpoint: false,
            },
            shared_hints(&AdmissionTuning::default()),
            FetchLimitsConfig {
                max_connections: 8,
                max_metadata_bytes: 1024 * 1024,
                max_inflight_metadata_bytes: 1024 * 1024,
            },
            test_tuning(),
        ));
        assert_eq!(f.process(&item(k)).await, FetchOutcome::NoPeers);

        // A peer that serves the wrong data: the fetch fails.
        let wrong = seeder(&first).await;
        let other = DhtKey([8; 20]);
        store.enqueue(other);
        let f = fetcher(&store, FixedPeers(vec![wrong]));
        assert_eq!(f.process(&item(other)).await, FetchOutcome::FetchFailed);
        assert_eq!(store.failures(&other), 1);
        // The refused address is now in the negative cache for this fetcher.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        let f = fetcher(&store, FixedPeers(vec![dead_addr]));
        assert_eq!(f.process(&item(other)).await, FetchOutcome::FetchFailed);
        assert_eq!(
            f.limiter()
                .try_acquire(dead_addr, Instant::now())
                .unwrap_err(),
            DestDenied::NegativeCache
        );
    }

    #[tokio::test]
    async fn process_batch_matches_process() {
        let first = info_dict("batch one", &["a.txt"], false);
        let second = info_dict("batch two", &["b.txt"], false);
        let private = info_dict("batch private", &["c.txt"], true);
        let seeders = [
            seeder(&first).await,
            seeder(&second).await,
            seeder(&private).await,
        ];
        // Every key sees the same peers, in order: each key skips the
        // seeders holding other torrents (mismatch) until its own. The last
        // key matches nothing and fails.
        let peers = FixedPeers(seeders.to_vec());
        let keys = [
            key_of(&first),
            key_of(&second),
            key_of(&private),
            DhtKey([77; 20]),
        ];
        let expected = [
            FetchOutcome::Ok,
            FetchOutcome::Ok,
            FetchOutcome::Private,
            FetchOutcome::FetchFailed,
        ];
        let store = MemoryStore::new();
        for k in &keys {
            store.enqueue(*k);
        }
        let f = fetcher(&store, peers);
        let items: Vec<PendingItem> = keys.iter().map(|k| item(*k)).collect();
        f.process_batch(&items).await;
        for (k, name) in [(keys[0], "batch one"), (keys[1], "batch two")] {
            let stored = store.torrent(&k).unwrap();
            assert_eq!(stored.name, name);
        }
        assert!(store.torrent(&keys[2]).is_none());
        assert_eq!(store.failures(&keys[3]), 1);
        // The failed key stays queued for retry; the rest are done.
        assert_eq!(store.pending_keys(), vec![keys[3]]);

        // One key at a time gives the same outcomes on the same setup.
        let store = MemoryStore::new();
        for k in &keys {
            store.enqueue(*k);
        }
        let f = fetcher(&store, FixedPeers(seeders.to_vec()));
        for (k, outcome) in keys.iter().zip(expected) {
            assert_eq!(f.process(&item(*k)).await, outcome);
        }
    }

    #[tokio::test]
    async fn unusable_verified_metadata_gives_up_at_once() {
        let store = MemoryStore::new();
        // Parses to a total size above i64::MAX: the store refuses it.
        let huge = info_dict_sized("huge", &[("a", i64::MAX), ("b", i64::MAX)], false);
        // Does not parse (a negative length).
        let broken = info_dict_sized("broken", &[("a", 5), ("b", -1)], false);
        for (info, expected) in [
            (&huge, FetchOutcome::StoreError),
            (&broken, FetchOutcome::ParseError),
        ] {
            let k = DhtKey(sha1(info));
            store.enqueue(k);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(dc3_peer::seeder::serve(listener, k, info.clone()));
            let f = fetcher(&store, FixedPeers(vec![addr]));
            assert_eq!(f.process(&item(k)).await, expected);
            assert!(store.torrent(&k).is_none());
            assert_eq!(store.failures(&k), 1);
            // Given up: never claimed again.
            assert!(!store.pending_keys().contains(&k), "{expected:?}");
        }
    }

    #[tokio::test]
    async fn transient_store_error_records_a_failure() {
        let store = MemoryStore::new();
        let good = info_dict("transient test", &["a.txt"], false);
        let k = key_of(&good);
        store.enqueue(k);
        // The database hiccups mid-write: `complete` fails transiently.
        store.fail_next_completes(1);
        let f = fetcher(&store, FixedPeers(vec![seeder(&good).await]));
        assert_eq!(f.process(&item(k)).await, FetchOutcome::StoreError);
        assert!(store.torrent(&k).is_none());
        // The failure went through `fail`, so `attempts` advances toward
        // `MAX_FETCH_ATTEMPTS` instead of stalling until lease expiry.
        assert_eq!(store.failures(&k), 1);
        // Still queued for retry, not given up.
        assert!(store.pending_keys().contains(&k));
    }

    #[tokio::test]
    async fn hints_are_tried_and_workers_drain_the_queue() {
        let info = info_dict("hinted", &["a.txt", "b.txt"], false);
        let k = key_of(&info);
        let addr = seeder(&info).await;
        let store = MemoryStore::new();
        let f = fetcher(&store, FixedPeers(Vec::new()));
        lock(&f.hints).add(k, addr, Instant::now());
        store.enqueue(k);
        let stop = CancellationToken::new();
        let worker = tokio::spawn(Arc::clone(&f).run_worker(stop.clone()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while store.torrent(&k).is_none() {
            assert!(
                Instant::now() < deadline,
                "the worker did not store the torrent"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
    }

    /// A lookup that finds no peers but carries one seed's filter (§8: the
    /// traversal was paid for, the estimate is free).
    #[derive(Clone)]
    struct FilteredPeers;

    impl PeerSource for FilteredPeers {
        async fn get_peers(&self, _: DhtKey, _: Duration) -> Vec<SocketAddr> {
            Vec::new()
        }
        fn own_ips(&self) -> Vec<IpAddr> {
            Vec::new()
        }
        fn own_endpoints(&self) -> Vec<SocketAddr> {
            Vec::new()
        }
        async fn scrape_peers(&self, _: DhtKey, _: Duration) -> dc3_dht::ScrapeReport {
            let mut sd = dc3_dht::bloom::ScrapeBloom::empty();
            sd.insert_ip(&"127.0.0.1".parse().unwrap());
            dc3_dht::ScrapeReport {
                peers: Vec::new(),
                seed_filters: vec![sd.0],
                peer_filters: vec![[0u8; dc3_dht::bloom::BLOOM_LEN]],
                aware: 1,
                unaware: 0,
                families_attempted: 2,
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_piggyback_records_the_free_estimate() {
        let store = MemoryStore::new();
        let k = DhtKey([7; 20]);
        store.enqueue(k);
        let f = fetcher(&store, FilteredPeers);
        // No peers, so the fetch fails — but the free estimate is kept for
        // the retry's liveness ordering.
        assert_eq!(f.process(&item(k)).await, FetchOutcome::NoPeers);
        assert_eq!(store.pending_seeders(&k), Some(1));
    }
}
