//! The crawl role (design §13): a DHT node, admission and fetch workers.
//!
//! Shutdown, when the token is cancelled:
//! 1. workers stop claiming new keys;
//! 2. fetches in progress get up to 30 s, then are abandoned (their leases
//!    expire and the keys are claimed again later);
//! 3. admission writes its last batch;
//! 4. the DHT node stops and saves its state.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dc3_dht::{DhtConfig, DhtStatsSnapshot, DhtTuning, Family, MIN_GOOD_NODES};
use dc3_policy::TermMatcher;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::admission::{Admission, AdmissionTuning, DISCOVERED_CHANNEL_CAPACITY, shared_hints};
use crate::config::{Config, ConfigError};
use crate::fetch::{FetchLimitsConfig, FetchTuning, Fetcher};
use crate::peers::PeerFilter;
use crate::stores::CrawlStore;

/// How often DHT counters are exported.
pub const DHT_STATS_INTERVAL: Duration = Duration::from_secs(5);
/// How often the queue depth is read.
pub const QUEUE_DEPTH_INTERVAL: Duration = Duration::from_secs(30);
/// How often readiness is re-checked.
pub const READINESS_INTERVAL: Duration = Duration::from_secs(5);
/// Time allowed for the readiness database ping.
pub const READINESS_PING_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for fetches in progress.
pub const SHUTDOWN_FETCH_WAIT: Duration = Duration::from_secs(30);

const METRIC_QUEUE_DEPTH: &str = "dc3_queue_depth";

/// Everything the crawl role needs besides the store and the policy.
#[derive(Debug, Clone)]
pub struct CrawlOptions {
    /// The DHT node. Tests set `allow_private_addrs`, `tuning` (with
    /// `limits_by_endpoint`) and the bind addresses here.
    pub dht: DhtConfig,
    /// `crawl.max_pending`.
    pub max_pending: i64,
    /// `crawl.fetch_workers`.
    pub fetch_workers: usize,
    /// Connection and byte limits of the fetchers.
    pub limits: FetchLimitsConfig,
    /// The peer address filter; [`CrawlOptions::new`] derives it from the
    /// DHT's test switches.
    pub filter: PeerFilter,
    pub admission: AdmissionTuning,
    pub fetch: FetchTuning,
    pub stats_interval: Duration,
    pub queue_depth_interval: Duration,
    pub readiness_interval: Duration,
    /// Good routing nodes needed for readiness.
    pub min_good_nodes: usize,
    pub shutdown_wait: Duration,
}

impl CrawlOptions {
    /// Production timings around `dht`. The peer filter follows
    /// `dht.allow_private_addrs` and `dht.tuning.limits_by_endpoint`.
    pub fn new(
        dht: DhtConfig,
        max_pending: i64,
        fetch_workers: usize,
        limits: FetchLimitsConfig,
    ) -> Self {
        let filter = PeerFilter {
            allow_private: dht.allow_private_addrs,
            by_endpoint: dht.tuning.limits_by_endpoint,
        };
        Self {
            dht,
            max_pending,
            fetch_workers,
            limits,
            filter,
            admission: AdmissionTuning::default(),
            fetch: FetchTuning::default(),
            stats_interval: DHT_STATS_INTERVAL,
            queue_depth_interval: QUEUE_DEPTH_INTERVAL,
            readiness_interval: READINESS_INTERVAL,
            min_good_nodes: MIN_GOOD_NODES,
            shutdown_wait: SHUTDOWN_FETCH_WAIT,
        }
    }

    /// Options from `[crawl]`.
    pub fn from_config(cfg: &Config) -> Result<Self, ConfigError> {
        let c = &cfg.crawl;
        let (bind_v4, bind_v6) = cfg.dht_binds()?;
        let dht = DhtConfig {
            bind_v4,
            bind_v6,
            bootstrap: c.bootstrap.clone(),
            state_file: cfg.dht_state_file(),
            max_packets_per_sec: c.max_packets_per_sec,
            sampler: true,
            sampler_concurrency: c.sampler_concurrency,
            read_only: c.read_only,
            allow_private_addrs: false,
            client_version: dc3_dht::DEFAULT_CLIENT_VERSION,
            tuning: DhtTuning::default(),
        };
        Ok(Self::new(
            dht,
            c.max_pending,
            c.fetch_workers,
            FetchLimitsConfig {
                max_connections: c.max_connections,
                max_metadata_bytes: c.max_metadata_bytes,
                max_inflight_metadata_bytes: c.max_inflight_metadata_bytes,
            },
        ))
    }
}

/// Why the crawl role failed.
#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    #[error("cannot start the DHT node: {0}")]
    Dht(#[from] dc3_dht::Error),
    #[error("crawl task failed: {0}")]
    Task(String),
}

/// A running crawl role.
#[derive(Debug)]
pub struct Crawler {
    dht: dc3_dht::Dht,
    supervisor: JoinHandle<Result<(), CrawlError>>,
}

impl Crawler {
    /// Starts the DHT node, admission, the fetch workers and the exporters.
    /// Everything stops when `cancel` is cancelled; [`Crawler::join`]
    /// waits for that.
    pub async fn start<S: CrawlStore>(
        opts: CrawlOptions,
        store: S,
        policy: Arc<TermMatcher>,
        ready: Arc<AtomicBool>,
        cancel: CancellationToken,
    ) -> Result<Crawler, CrawlError> {
        let (tx, rx) = mpsc::channel(DISCOVERED_CHANNEL_CAPACITY);
        let dht = dc3_dht::Dht::start(opts.dht.clone(), tx).await?;
        tracing::info!(
            sockets = dht.local_addrs().len(),
            sampler = opts.dht.sampler,
            read_only = opts.dht.read_only,
            workers = opts.fetch_workers,
            "DHT node started"
        );
        let stop = cancel.child_token();
        let hints = shared_hints(&opts.admission);

        let admission = Admission::new(
            store.clone(),
            opts.max_pending,
            opts.filter,
            opts.admission,
            Arc::clone(&hints),
            Instant::now(),
        );
        let admission_stop = CancellationToken::new();
        let admission_task = tokio::spawn(admission.run(rx, dht.clone(), admission_stop.clone()));

        let fetcher = Arc::new(Fetcher::new(
            store.clone(),
            dht.clone(),
            policy,
            opts.filter,
            hints,
            opts.limits,
            opts.fetch,
        ));
        let mut workers = JoinSet::new();
        for _ in 0..opts.fetch_workers {
            workers.spawn(Arc::clone(&fetcher).run_worker(stop.clone()));
        }

        let mut aux = JoinSet::new();
        aux.spawn(export_dht_stats(
            dht.clone(),
            opts.stats_interval,
            stop.clone(),
        ));
        aux.spawn(export_queue_depth(
            store.clone(),
            opts.queue_depth_interval,
            stop.clone(),
        ));
        aux.spawn(track_readiness(
            store,
            dht.clone(),
            opts.min_good_nodes,
            Arc::clone(&ready),
            opts.readiness_interval,
            stop.clone(),
        ));

        let supervisor = tokio::spawn(supervise(Supervised {
            dht: dht.clone(),
            stop,
            workers,
            admission_stop,
            admission_task,
            aux,
            shutdown_wait: opts.shutdown_wait,
            ready,
        }));
        Ok(Crawler { dht, supervisor })
    }

    /// The DHT node (it stops when the crawler stops).
    pub fn dht(&self) -> &dc3_dht::Dht {
        &self.dht
    }

    /// Waits until the crawler has stopped.
    pub async fn join(self) -> Result<(), CrawlError> {
        self.supervisor
            .await
            .map_err(|e| CrawlError::Task(e.to_string()))?
    }
}

/// Runs the crawl role until `cancel` is cancelled.
pub async fn run<S: CrawlStore>(
    opts: CrawlOptions,
    store: S,
    policy: Arc<TermMatcher>,
    ready: Arc<AtomicBool>,
    cancel: CancellationToken,
) -> Result<(), CrawlError> {
    Crawler::start(opts, store, policy, ready, cancel)
        .await?
        .join()
        .await
}

struct Supervised {
    dht: dc3_dht::Dht,
    stop: CancellationToken,
    workers: JoinSet<()>,
    admission_stop: CancellationToken,
    admission_task: JoinHandle<()>,
    aux: JoinSet<()>,
    shutdown_wait: Duration,
    ready: Arc<AtomicBool>,
}

async fn supervise(mut s: Supervised) -> Result<(), CrawlError> {
    let admission_ended = tokio::select! {
        () = s.stop.cancelled() => false,
        _ = &mut s.admission_task => true,
    };
    s.stop.cancel();
    s.ready.store(false, Ordering::Relaxed);
    tracing::info!("crawl role stopping");

    let deadline = Instant::now() + s.shutdown_wait;
    loop {
        match tokio::time::timeout_at(deadline, s.workers.join_next()).await {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(e))) => tracing::error!(error = %e, "a fetch worker failed"),
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(
                    workers = s.workers.len(),
                    "abandoning fetches still in progress"
                );
                s.workers.shutdown().await;
                break;
            }
        }
    }

    if !admission_ended {
        s.admission_stop.cancel();
        if let Err(e) = (&mut s.admission_task).await {
            tracing::error!(error = %e, "the admission task failed");
        }
    }
    s.aux.shutdown().await;
    // The readiness task may have been stopped between its checks.
    s.ready.store(false, Ordering::Relaxed);
    s.dht.shutdown().await;
    tracing::info!("crawl role stopped");
    if admission_ended {
        return Err(CrawlError::Task("admission stopped unexpectedly".into()));
    }
    Ok(())
}

/// Exports one DHT snapshot as the metrics in `docs/04-operations.md` §6.
pub fn export_snapshot(s: &DhtStatsSnapshot) {
    for family in Family::ALL {
        let f = s.family(family);
        let label = family.as_str();
        metrics::counter!("dc3_dht_packets_in_total", "family" => label).absolute(f.packets_in);
        metrics::counter!("dc3_dht_packets_out_total", "family" => label).absolute(f.packets_out);
        for (reason, n) in f.dropped.iter() {
            metrics::counter!(
                "dc3_dht_packets_dropped_total",
                "family" => label,
                "reason" => reason.as_str()
            )
            .absolute(n);
        }
        metrics::counter!("dc3_dht_samples_total", "family" => label).absolute(f.samples);
        metrics::gauge!("dc3_dht_routing_nodes", "family" => label).set(f.routing_nodes as f64);
        metrics::gauge!("dc3_dht_good_nodes", "family" => label).set(f.good_nodes as f64);
    }
    for (method, n) in s.queries_received.iter() {
        metrics::counter!("dc3_dht_queries_received_total", "method" => method).absolute(n);
    }
    metrics::counter!("dc3_dht_timeouts_total").absolute(s.timeouts);
    metrics::counter!("dc3_dht_sampler_early_total").absolute(s.sampler_early);
    metrics::counter!("dc3_dht_sampler_visited_full_total").absolute(s.sampler_visited_full);
    metrics::counter!("dc3_dht_responder_dropped_total").absolute(s.responder_dropped);
    metrics::counter!("dc3_dht_discovered_dropped_total").absolute(s.discovered_dropped);
    metrics::gauge!("dc3_dht_peer_store_keys").set(s.peer_store_keys as f64);
}

async fn export_dht_stats(dht: dc3_dht::Dht, every: Duration, stop: CancellationToken) {
    let mut ticker = tokio::time::interval(every.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            _ = ticker.tick() => export_snapshot(&dht.stats()),
        }
    }
}

async fn export_queue_depth<S: CrawlStore>(store: S, every: Duration, stop: CancellationToken) {
    let mut ticker = tokio::time::interval(every.max(Duration::from_millis(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            _ = ticker.tick() => match store.pending_depth().await {
                Ok(depth) => metrics::gauge!(METRIC_QUEUE_DEPTH).set(depth as f64),
                Err(e) => tracing::warn!(error = %e, "reading the queue depth failed"),
            },
        }
    }
}

async fn track_readiness<S: CrawlStore>(
    store: S,
    dht: dc3_dht::Dht,
    min_good_nodes: usize,
    ready: Arc<AtomicBool>,
    every: Duration,
    stop: CancellationToken,
) {
    loop {
        let database = matches!(
            tokio::time::timeout(READINESS_PING_TIMEOUT, store.ping()).await,
            Ok(Ok(()))
        );
        let routing = dht.good_nodes() >= min_good_nodes;
        ready.store(
            database && routing && !stop.is_cancelled(),
            Ordering::Relaxed,
        );
        tokio::select! {
            () = stop.cancelled() => break,
            () = tokio::time::sleep(every) => {}
        }
    }
    ready.store(false, Ordering::Relaxed);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn options_from_config() {
        let cfg = Config::default();
        let o = CrawlOptions::from_config(&cfg).unwrap();
        assert_eq!(o.dht.bind_v4, Some("0.0.0.0:6881".parse().unwrap()));
        assert_eq!(o.dht.bind_v6, Some("[::]:6881".parse().unwrap()));
        assert_eq!(
            o.dht.state_file,
            Some("/var/lib/dhtcrawler3/dht-state.json".into())
        );
        assert_eq!(o.dht.bootstrap.len(), 4);
        assert!(o.dht.sampler);
        assert_eq!(o.dht.sampler_concurrency, 32);
        assert!(!o.dht.allow_private_addrs);
        assert_eq!(o.dht.tuning, DhtTuning::default());
        assert!(o.dht.validate().is_ok());
        assert_eq!(o.filter, PeerFilter::PRODUCTION);
        assert_eq!(o.fetch_workers, 64);
        assert_eq!(o.max_pending, 5_000_000);
        assert_eq!(o.limits.max_connections, 256);
        assert_eq!(o.limits.max_inflight_metadata_bytes, 268_435_456);
        assert_eq!(o.min_good_nodes, MIN_GOOD_NODES);

        let mut cfg = Config::default();
        cfg.crawl.bind_v6 = String::new();
        cfg.crawl.read_only = true;
        cfg.crawl.state_dir = std::path::PathBuf::new();
        let o = CrawlOptions::from_config(&cfg).unwrap();
        assert_eq!(o.dht.bind_v6, None);
        assert!(o.dht.read_only);
        assert_eq!(o.dht.state_file, None);

        let test_dht = DhtConfig {
            allow_private_addrs: true,
            tuning: DhtTuning {
                limits_by_endpoint: true,
                ..DhtTuning::default()
            },
            ..DhtConfig::default()
        };
        let o = CrawlOptions::new(test_dht, 10, 1, o.limits);
        assert!(o.filter.allow_private && o.filter.by_endpoint);
    }

    #[test]
    fn snapshot_export_does_not_need_a_recorder() {
        export_snapshot(&DhtStatsSnapshot::default());
    }
}
