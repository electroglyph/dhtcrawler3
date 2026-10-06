//! Fetch workers (design §13): claim a key, find peers, fetch its metadata
//! over BEP 9, verify and parse it, apply the policy, and store the result.
//!
//! Each worker:
//! 1. claims one key with a 120 s lease, renewed every 90 s while it works;
//! 2. collects peers from the hint map and `get_peers` (15 s), each passing
//!    the address chokepoint and the per-destination limits;
//! 3. tries up to 8 peers, 3 at a time, inside the global connection limit
//!    and the metadata byte budget, all within 60 s;
//! 4. verifies, parses and checks the name and every path against the
//!    blocked terms;
//! 5. finishes with `complete`, `fail`, `give_up` (verified metadata that
//!    cannot be parsed or stored) or `deny` (`csam-auto` for blocked
//!    terms, `private` for BEP 27 torrents).

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use dc3_core::DhtKey;
use dc3_dht::compact::canonical_addr;
use dc3_peer::{BYTE_BUDGET_UNIT, FetchError, FetchLimits};
use dc3_policy::TermMatcher;
use dc3_store::{DenyReason, FileRow, NewTorrent, PendingItem, StoreError};
use dc3_torrent::TorrentMeta;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::admission::{METRIC_BLOCKED, SharedHints, take_hints};
use crate::peers::{OwnAddrs, PeerFilter, PeerSource};
use crate::stores::CrawlStore;

/// Queue items claimed at a time by one worker.
pub const CLAIM_BATCH: i64 = 1;
/// Lease on a claimed key.
pub const CLAIM_LEASE: Duration = Duration::from_secs(120);
/// How often a lease is renewed while its key is being worked on.
pub const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(90);
/// Shortest pause of an idle worker.
pub const IDLE_SLEEP_MIN: Duration = Duration::from_secs(1);
/// Longest pause of an idle worker.
pub const IDLE_SLEEP_MAX: Duration = Duration::from_secs(3);
/// Time allowed for the `get_peers` lookup of one key.
pub const GET_PEERS_TIMEOUT: Duration = Duration::from_secs(15);
/// Time allowed for all fetch attempts of one key.
pub const KEY_DEADLINE: Duration = Duration::from_secs(60);
/// Peers tried per key.
pub const MAX_PEER_ATTEMPTS: usize = 8;
/// Peers tried at the same time per key.
pub const PARALLEL_ATTEMPTS: usize = 3;
/// TCP connect timeout per peer.
pub const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Handshake timeout per peer.
pub const PEER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Whole-fetch timeout per peer.
pub const PEER_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
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
/// Actor recorded for the crawler's own denials.
pub const DENY_ACTOR: &str = "crawler";
/// Note recorded when a name or path matched a blocked term.
pub const BLOCKED_TERM_NOTE: &str = "blocked term";
/// Note recorded for BEP 27 private torrents.
pub const PRIVATE_NOTE: &str = "BEP 27 private torrent";

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
    /// The name or a path matched a blocked term; the key was denied.
    Blocked,
    /// A BEP 27 private torrent; the key was denied.
    Private,
    /// The store refused it: a key is on the denylist.
    Denied,
    /// The final database operation failed.
    StoreError,
}

impl FetchOutcome {
    /// Every outcome.
    pub const ALL: [FetchOutcome; 8] = [
        FetchOutcome::Ok,
        FetchOutcome::NoPeers,
        FetchOutcome::FetchFailed,
        FetchOutcome::ParseError,
        FetchOutcome::Blocked,
        FetchOutcome::Private,
        FetchOutcome::Denied,
        FetchOutcome::StoreError,
    ];

    /// The metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            FetchOutcome::Ok => "ok",
            FetchOutcome::NoPeers => "no_peers",
            FetchOutcome::FetchFailed => "fetch_failed",
            FetchOutcome::ParseError => "parse_error",
            FetchOutcome::Blocked => "blocked",
            FetchOutcome::Private => "private",
            FetchOutcome::Denied => "denied",
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
    /// The name or a path matched a blocked term (checked even when parsing
    /// failed part-way, since the visitor saw those entries).
    Blocked,
    /// It could not be parsed.
    Invalid,
    /// A BEP 27 private torrent.
    Private,
    /// A torrent that may be stored.
    Valid(TorrentMeta),
}

/// Verifies `info` against `key`, parses it, and matches the name and every
/// parsed path against `policy`. CPU-bound; run it off the async threads.
pub fn inspect(key: &DhtKey, info: &[u8], policy: &TermMatcher) -> Inspection {
    if dc3_torrent::verify(key, info).is_none() {
        return Inspection::Mismatch;
    }
    let mut blocked = false;
    let parsed = dc3_torrent::parse_info_visit(info, &mut |text| {
        if !blocked && policy.matches_affixed(text) {
            blocked = true;
        }
    });
    if blocked {
        return Inspection::Blocked;
    }
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
    policy: Arc<TermMatcher>,
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

impl<S: CrawlStore, P: PeerSource> Fetcher<S, P> {
    /// Shared state for the workers.
    pub fn new(
        store: S,
        peers: P,
        policy: Arc<TermMatcher>,
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
            policy,
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

    /// Fetches, checks and stores one claimed key.
    pub async fn process(&self, item: &PendingItem) -> FetchOutcome {
        let key = item.dht_key;
        let renewal = RenewalGuard::spawn(
            self.store.clone(),
            key,
            self.tuning.lease,
            self.tuning.renew_every,
        );
        let obtained = tokio::time::timeout(self.tuning.key_deadline, self.obtain(key))
            .await
            .unwrap_or(Obtained::Failed);
        let outcome = match obtained {
            Obtained::NoPeers => self.give_back(key, FetchOutcome::NoPeers).await,
            Obtained::Failed => self.give_back(key, FetchOutcome::FetchFailed).await,
            Obtained::Metadata(info) => self.store_metadata(key, info).await,
        };
        drop(renewal);
        metrics::counter!(METRIC_FETCH, "outcome" => outcome.as_str()).increment(1);
        tracing::debug!(
            outcome = outcome.as_str(),
            earlier_attempts = item.attempts,
            "fetch finished"
        );
        outcome
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
        loop {
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
                    }
                }
            }
            if running.is_empty() && (lookup_done || attempts >= self.tuning.max_attempts) {
                break;
            }
            tokio::select! {
                report = &mut lookup, if !lookup_done => {
                    lookup_done = true;
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

    async fn store_metadata(&self, key: DhtKey, info: Vec<u8>) -> FetchOutcome {
        let policy = Arc::clone(&self.policy);
        let inspection = tokio::task::spawn_blocking(move || inspect(&key, &info, &policy))
            .await
            .unwrap_or(Inspection::Invalid);
        match inspection {
            Inspection::Mismatch => self.give_back(key, FetchOutcome::FetchFailed).await,
            // Verified metadata that does not parse never will: stop now.
            Inspection::Invalid => self.give_up(key, FetchOutcome::ParseError).await,
            Inspection::Blocked => {
                metrics::counter!(METRIC_BLOCKED, "reason" => "blocked_term").increment(1);
                self.deny(
                    key,
                    DenyReason::CsamAuto,
                    BLOCKED_TERM_NOTE,
                    FetchOutcome::Blocked,
                )
                .await
            }
            Inspection::Private => {
                metrics::counter!(METRIC_BLOCKED, "reason" => "private").increment(1);
                self.deny(
                    key,
                    DenyReason::Private,
                    PRIVATE_NOTE,
                    FetchOutcome::Private,
                )
                .await
            }
            Inspection::Valid(meta) => {
                match self.store.complete(&key, &new_torrent(key, meta)).await {
                    Ok(_) => FetchOutcome::Ok,
                    Err(StoreError::Denied) => FetchOutcome::Denied,
                    // The store can never accept this torrent (e.g. a size
                    // above i64::MAX); retrying would loop on it forever.
                    Err(e @ StoreError::Invalid(_)) => {
                        tracing::warn!(error = %e, "fetched torrent cannot be stored; giving up");
                        self.give_up(key, FetchOutcome::StoreError).await
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "storing a fetched torrent failed");
                        FetchOutcome::StoreError
                    }
                }
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

    async fn deny(
        &self,
        key: DhtKey,
        reason: DenyReason,
        note: &str,
        outcome: FetchOutcome,
    ) -> FetchOutcome {
        match self
            .store
            .deny(key.as_bytes(), reason, Some(note), DENY_ACTOR)
            .await
        {
            Ok(_) => outcome,
            Err(e) => {
                tracing::warn!(error = %e, "denying a fetched torrent failed");
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
                    for item in &items {
                        self.process(item).await;
                    }
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
                "blocked",
                "private",
                "denied",
                "store_error"
            ]
        );
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

    fn seed_policy() -> Arc<TermMatcher> {
        Arc::new(dc3_policy::try_seed().unwrap())
    }

    #[test]
    fn inspection() {
        let policy = seed_policy();
        let good = info_dict("ubuntu images", &["a/one.iso", "b/two.iso"], false);
        let k = key_of(&good);
        match inspect(&k, &good, &policy) {
            Inspection::Valid(meta) => {
                assert_eq!(meta.name, "ubuntu images");
                let t = new_torrent(k, meta);
                assert_eq!(t.files.len(), 2);
                assert_eq!(t.total_size, 2000);
                assert_eq!(t.info_hash_v1, Some(k));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            inspect(&DhtKey([0; 20]), &good, &policy),
            Inspection::Mismatch
        );
        let named = info_dict("some PTHC stuff", &["x.txt"], false);
        assert_eq!(
            inspect(&key_of(&named), &named, &policy),
            Inspection::Blocked
        );
        let pathed = info_dict(
            "holiday",
            &["ok.txt", "deep/p.t.h.c/x.txt", "hussyfan/y"],
            false,
        );
        assert_eq!(
            inspect(&key_of(&pathed), &pathed, &policy),
            Inspection::Blocked
        );
        let private = info_dict("tracker only", &["x.txt"], true);
        assert_eq!(
            inspect(&key_of(&private), &private, &policy),
            Inspection::Private
        );
        // The test SHA-1 agrees with dc3-torrent.
        assert_eq!(DhtKey(sha1(&good)), k);
        assert_eq!(
            DhtKey(sha1(b"abc")).to_hex(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // Verified but unparseable: not a torrent, or a negative length.
        let junk = dc3_bencode::encode(&OwnedValue::Dict(BTreeMap::new()));
        assert_eq!(
            inspect(&DhtKey(sha1(&junk)), &junk, &policy),
            Inspection::Invalid
        );
        let negative = info_dict_sized("clean name", &[("a.txt", 5), ("b.txt", -1)], false);
        assert_eq!(
            inspect(&DhtKey(sha1(&negative)), &negative, &policy),
            Inspection::Invalid
        );
        // A blocked term seen before the parse error still blocks.
        let both = info_dict_sized("clean", &[("pthc.txt", 5), ("b.txt", -1)], false);
        assert_eq!(
            inspect(&DhtKey(sha1(&both)), &both, &policy),
            Inspection::Blocked
        );
        // Short seeds hidden in affixes block too, matching display.
        let affixed_name = info_dict("xpthc movie", &["ok.txt"], false);
        assert_eq!(
            inspect(&key_of(&affixed_name), &affixed_name, &policy),
            Inspection::Blocked
        );
        let affixed_path = info_dict("holiday", &["xpthc/a.jpg"], false);
        assert_eq!(
            inspect(&key_of(&affixed_path), &affixed_path, &policy),
            Inspection::Blocked
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
        let filter = PeerFilter {
            allow_private: true,
            by_endpoint: true,
        };
        Arc::new(Fetcher::new(
            store.clone(),
            peers,
            seed_policy(),
            filter,
            shared_hints(&AdmissionTuning::default()),
            FetchLimitsConfig {
                max_connections: 8,
                max_metadata_bytes: 1024 * 1024,
                max_inflight_metadata_bytes: 1024 * 1024,
            },
            test_tuning(),
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

    #[tokio::test]
    async fn process_stores_denies_and_fails() {
        let good = info_dict("fetch test", &["a.txt", "b/c.txt"], false);
        let blocked = info_dict("fetch test", &["pthc/c.txt"], false);
        let private = info_dict("private test", &["a.txt"], true);
        let seeders = [
            seeder(&good).await,
            seeder(&blocked).await,
            seeder(&private).await,
        ];
        let store = MemoryStore::new();
        for (info, addr, expected) in [
            (&good, seeders[0], FetchOutcome::Ok),
            (&blocked, seeders[1], FetchOutcome::Blocked),
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
        let good_key = key_of(&good);
        let stored = store.torrent(&good_key).unwrap();
        assert_eq!(stored.name, "fetch test");
        assert_eq!(stored.files.len(), 2);
        let denial = store.denial(key_of(&blocked).as_bytes()).unwrap();
        assert_eq!(denial.reason, DenyReason::CsamAuto);
        assert_eq!(denial.actor, DENY_ACTOR);
        assert_eq!(denial.note.as_deref(), Some(BLOCKED_TERM_NOTE));
        assert!(store.torrent(&key_of(&blocked)).is_none());
        assert_eq!(
            store.denial(key_of(&private).as_bytes()).unwrap().reason,
            DenyReason::Private
        );
        assert!(store.pending_keys().is_empty());

        // A denied key is refused by the store.
        let again = info_dict("denied later", &["a.txt"], false);
        let k = key_of(&again);
        store.preload_denial(k.as_bytes(), DenyReason::Dmca);
        let f = fetcher(&store, FixedPeers(vec![seeder(&again).await]));
        assert_eq!(f.process(&item(k)).await, FetchOutcome::Denied);

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
            seed_policy(),
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
        let wrong = seeder(&good).await;
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
