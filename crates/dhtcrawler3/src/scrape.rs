//! BEP 33 scrape worker (bep33.md §4): re-polls stored torrents for their
//! current seeder estimate and tombstones dead swarms.
//!
//! Each worker loops: claim due rows, scrape each key with [`Dht::scrape`],
//! classify the report, and write back the outcome. Classification:
//!
//! * **live** (an aware estimate above the threshold): store the estimate,
//!   reset failures. A live verdict from one family is safe: the union is a
//!   lower bound, and one live proof suffices.
//! * **dead** (aware, estimate at or below the threshold): bump failures;
//!   tombstone once they reach the maximum. A dead verdict is only valid
//!   when **both** families were attempted in this round: a v4-only zero
//!   plus a skipped v6 must classify as unknown, never dead.
//! * **unknown** (no aware responses, a saturated union, or a single-family
//!   zero): keep the old estimate and failures, refresh the stamp so the
//!   row is not re-polled in a tight loop. Unknown never kills.
//!
//! The tombstone is conditional on the row being unchanged since the claim,
//! so a concurrent fetch wins the race. A periodic sweep purges old scrape
//! tombstones (never denylist ones) and trims removal memory.

use std::sync::Arc;
use std::time::Duration;

use dc3_dht::{Dht, ScrapeReport};
use dc3_store::ScrapeItem;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::CrawlConfig;
use crate::stores::CrawlStore;

/// Shortest pause of an idle scrape worker.
pub const SCRAPE_IDLE_MIN: Duration = Duration::from_secs(60);
/// Longest pause of an idle scrape worker.
pub const SCRAPE_IDLE_MAX: Duration = Duration::from_secs(600);
/// Cap of `removed_keys` rows (bep33.md §4a: ~1M LRU).
pub const SCRAPE_REMOVED_KEYS_CAP: i64 = dc3_store::MAX_REMOVED_KEYS;
/// Most tombstones one sweep deletes (bounds one sweep's write load; the
/// rest go on the next sweep, biggest first).
pub const MAX_PURGE_BATCH: i64 = 10_000;

const METRIC_SCRAPES: &str = "dc3_scrapes_total";
const METRIC_TOMBSTONES: &str = "dc3_scrape_tombstones_total";
const METRIC_PURGED: &str = "dc3_purge_tombstoned_total";
const METRIC_DUE_DEPTH: &str = "dc3_scrape_due_depth";
const METRIC_REMOVED_KEYS: &str = "dc3_removed_keys_count";

/// Everything the scrape worker needs from `[crawl]`.
#[derive(Debug, Clone)]
pub struct ScrapeTuning {
    /// Rows claimed per round (`scrape_batch`).
    pub batch: i64,
    /// Consecutive dead scrapes before a tombstone (`max_scrape_failures`).
    pub max_failures: u32,
    /// A swarm is dead when its estimate is at most this
    /// (`scrape_seeder_threshold`).
    pub threshold: u64,
    /// Overall deadline of one scrape lookup (`scrape_lookup_timeout_secs`).
    pub lookup_timeout: Duration,
    /// In-flight scrape lookups per worker (`scrape_concurrency`).
    pub concurrency: usize,
    /// Re-scrape interval of rows with an estimate (`scrape_interval_secs`).
    pub live_interval: Duration,
    /// Re-scrape interval of rows without one (`scrape_unknown_interval_secs`).
    pub unknown_interval: Duration,
    /// Interval of the purge sweep (`scrape_sweep_secs`).
    pub sweep_interval: Duration,
    /// Grace before a tombstoned row is hard-purged (`tombstone_purge_hours`).
    pub purge_grace: Duration,
}

impl ScrapeTuning {
    /// Timings from `[crawl]`. The worker count lives in
    /// [`crate::crawl::Crawler::start`], which spawns the workers.
    pub fn from_config(c: &CrawlConfig) -> Self {        Self {
            batch: i64::try_from(c.scrape_batch).unwrap_or(i64::MAX),
            max_failures: c.max_scrape_failures,
            threshold: u64::from(c.scrape_seeder_threshold),
            lookup_timeout: Duration::from_secs(c.scrape_lookup_timeout_secs),
            concurrency: c.scrape_concurrency,
            live_interval: Duration::from_secs(c.scrape_interval_secs),
            unknown_interval: Duration::from_secs(c.scrape_unknown_interval_secs),
            sweep_interval: Duration::from_secs(c.scrape_sweep_secs),
            purge_grace: Duration::from_secs(c.tombstone_purge_hours.saturating_mul(3600)),
        }
    }
}

impl Default for ScrapeTuning {
    /// The spec defaults (a default `[crawl]` section).
    fn default() -> Self {
        Self::from_config(&CrawlConfig::default())
    }
}

/// The classification of one scrape report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrapeVerdict {
    /// An aware estimate above the threshold; carries the estimate.
    Live { est: u32 },
    /// An aware estimate at or below the threshold over both families;
    /// carries the estimate.
    Dead { est: u32 },
    /// No aware responses, a saturated union, or a single-family zero:
    /// keep the old estimate, change nothing.
    Unknown,
}

/// Classifies `report` against `threshold` (bep33.md §4 step 3).
pub fn classify(report: &ScrapeReport, threshold: u64) -> ScrapeVerdict {
    let Some(est) = report.seeders_est() else {
        return ScrapeVerdict::Unknown;
    };
    if est > threshold {
        return ScrapeVerdict::Live {
            est: u32::try_from(est).unwrap_or(u32::MAX),
        };
    }
    if report.families_attempted >= 2 {
        return ScrapeVerdict::Dead {
            est: u32::try_from(est).unwrap_or(u32::MAX),
        };
    }
    ScrapeVerdict::Unknown
}

/// One scrape worker: claims due rows, scrapes them, writes the outcomes,
/// and sweeps tombstones on `sweep_interval`.
#[derive(Debug)]
pub struct Scraper<S> {
    store: S,
    dht: Dht,
    tuning: ScrapeTuning,
}

impl<S: CrawlStore> Scraper<S> {
    /// A worker around `store` and the DHT node.
    pub fn new(store: S, dht: Dht, tuning: ScrapeTuning) -> Self {
        Self { store, dht, tuning }
    }

    /// Runs until `stop` is cancelled. Store errors are logged, never fatal:
    /// the next round retries.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        let mut idle = SCRAPE_IDLE_MIN;
        let mut last_sweep = tokio::time::Instant::now();
        loop {
            if stop.is_cancelled() {
                return;
            }
            if last_sweep.elapsed() >= self.tuning.sweep_interval {
                self.sweep().await;
                last_sweep = tokio::time::Instant::now();
            }
            let items = match self
                .store
                .claim_scrape_due(
                    self.tuning.batch,
                    self.tuning.live_interval,
                    self.tuning.unknown_interval,
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    tracing::error!(error = %e, "scrape claim failed");
                    idle = self.sleep(idle, &stop).await;
                    continue;
                }
            };
            // The last claim size, saturating at the batch: nonzero means
            // backlog, zero means the worker is idle.
            metrics::gauge!(METRIC_DUE_DEPTH).set(items.len() as f64);
            if items.is_empty() {
                idle = self.sleep(idle, &stop).await;
                continue;
            }
            idle = SCRAPE_IDLE_MIN;
            self.scrape_all(&items, &stop).await;
        }
    }

    /// Scrapes every claimed item with bounded concurrency.
    async fn scrape_all(self: &Arc<Self>, items: &[ScrapeItem], stop: &CancellationToken) {
        let semaphore = Arc::new(Semaphore::new(self.tuning.concurrency.max(1)));
        let mut in_flight = FuturesUnordered::new();
        for item in items {
            if stop.is_cancelled() {
                break;
            }
            let this = Arc::clone(self);
            let permit = Arc::clone(&semaphore);
            let item = item.clone();
            in_flight.push(async move {
                let _permit = permit.acquire_owned().await;
                this.scrape_one(&item).await;
            });
        }
        while in_flight.next().await.is_some() {}
    }

    /// Scrapes one claimed row and writes its outcome.
    async fn scrape_one(&self, item: &ScrapeItem) {
        // Skip rows denied or hidden since the claim: the claim filter
        // already excluded them, this only closes the race cheaply. (Hidden
        // rows have no cheap check; recording stats on one is harmless and
        // the tombstone below still cannot clobber fresh data.)
        match self.store.is_denied(&[item.dht_key.as_bytes()]).await {
            Ok(true) => return,
            Err(e) => {
                tracing::error!(error = %e, id = item.id, "scrape deny recheck failed");
                return;
            }
            Ok(false) => {}
        }
        let report = self.dht.scrape(item.dht_key, self.tuning.lookup_timeout).await;
        self.apply(item, classify(&report, self.tuning.threshold)).await;
    }

    /// Writes one verdict. Unknown keeps the old estimate and failures.
    async fn apply(&self, item: &ScrapeItem, verdict: ScrapeVerdict) {
        match verdict {
            ScrapeVerdict::Live { est } => {
                metrics::counter!(METRIC_SCRAPES, "outcome" => "live").increment(1);
                if let Err(e) = self.store.record_scrape(item.id, Some(est), 0).await {
                    tracing::error!(error = %e, id = item.id, "record_scrape failed");
                }
            }
            ScrapeVerdict::Dead { est } => {
                let failures = item.scrape_failures.saturating_add(1);
                if failures >= self.tuning.max_failures {
                    metrics::counter!(METRIC_SCRAPES, "outcome" => "dead").increment(1);
                    match self
                        .store
                        .tombstone_dead(item.id, item.last_seen_at, item.change_seq)
                        .await
                    {
                        Ok(true) => {
                            metrics::counter!(METRIC_TOMBSTONES).increment(1);
                        }
                        Ok(false) => {
                            // The row changed since the claim (a fetch won
                            // the race): record the scrape instead of
                            // wiping fresh data.
                            if let Err(e) =
                                self.store.record_scrape(item.id, Some(est), failures).await
                            {
                                tracing::error!(error = %e, id = item.id, "record_scrape failed");
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, id = item.id, "tombstone_dead failed");
                        }
                    }
                } else {
                    metrics::counter!(METRIC_SCRAPES, "outcome" => "dying").increment(1);
                    if let Err(e) = self
                        .store
                        .record_scrape(item.id, Some(est), failures)
                        .await
                    {
                        tracing::error!(error = %e, id = item.id, "record_scrape failed");
                    }
                }
            }
            ScrapeVerdict::Unknown => {
                metrics::counter!(METRIC_SCRAPES, "outcome" => "unknown").increment(1);
                if let Err(e) = self
                    .store
                    .record_scrape(item.id, item.seeders_est, item.scrape_failures)
                    .await
                {
                    tracing::error!(error = %e, id = item.id, "record_scrape failed");
                }
            }
        }
    }

    /// Purges old scrape tombstones and trims removal memory.
    async fn sweep(&self) {
        match self.store.purge_tombstoned(self.tuning.purge_grace, MAX_PURGE_BATCH).await {
            Ok(n) => {
                if n > 0 {
                    metrics::counter!(METRIC_PURGED).increment(n);
                    tracing::info!(purged = n, "scrape sweep purged tombstones");
                }
            }
            Err(e) => tracing::error!(error = %e, "purge_tombstoned failed"),
        }
        match self.store.trim_removed_keys(SCRAPE_REMOVED_KEYS_CAP).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!(trimmed = n, "scrape sweep trimmed removal memory");
                }
            }
            Err(e) => tracing::error!(error = %e, "trim_removed_keys failed"),
        }
        match self.store.removed_keys_count().await {
            Ok(n) => metrics::gauge!(METRIC_REMOVED_KEYS).set(n as f64),
            Err(e) => tracing::error!(error = %e, "removed_keys_count failed"),
        }
    }

    /// Sleeps with backoff between `SCRAPE_IDLE_MIN` and `SCRAPE_IDLE_MAX`;
    /// returns the next pause.
    async fn sleep(&self, pause: Duration, stop: &CancellationToken) -> Duration {
        tokio::select! {
            () = stop.cancelled() => {},
            () = tokio::time::sleep(pause) => {},
        }
        pause.saturating_mul(2).min(SCRAPE_IDLE_MAX).max(SCRAPE_IDLE_MIN)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn empty_report(aware: usize, families_attempted: usize) -> ScrapeReport {
        ScrapeReport {
            peers: Vec::new(),
            seed_filters: Vec::new(),
            peer_filters: Vec::new(),
            aware,
            unaware: 0,
            families_attempted,
        }
    }

    fn live_report(seeds: &[&str]) -> ScrapeReport {
        use std::net::IpAddr;
        let mut sd = dc3_dht::bloom::ScrapeBloom::empty();
        for s in seeds {
            sd.insert_ip(&s.parse::<IpAddr>().unwrap());
        }
        ScrapeReport {
            peers: Vec::new(),
            seed_filters: vec![sd.0],
            peer_filters: vec![[0u8; dc3_dht::bloom::BLOOM_LEN]],
            aware: 1,
            unaware: 0,
            families_attempted: 2,
        }
    }

    #[test]
    fn classify_maps_reports_to_verdicts() {
        // Live: aware estimate above the threshold.
        assert_eq!(
            classify(&live_report(&["9.9.9.9"]), 0),
            ScrapeVerdict::Live { est: 1 }
        );
        // Dead: aware estimate at or below the threshold over both families.
        let mut dead = live_report(&[]);
        dead.aware = 2;
        dead.seed_filters = vec![[0u8; dc3_dht::bloom::BLOOM_LEN]; 2];
        dead.peer_filters = vec![[0u8; dc3_dht::bloom::BLOOM_LEN]; 2];
        assert_eq!(classify(&dead, 0), ScrapeVerdict::Dead { est: 0 });
        // The same zero over one family is unknown, never dead.
        let mut single = dead.clone();
        single.families_attempted = 1;
        assert_eq!(classify(&single, 0), ScrapeVerdict::Unknown);
        // No aware responses: unknown.
        assert_eq!(classify(&empty_report(0, 2), 0), ScrapeVerdict::Unknown);
        // Zero families attempted with zero filters: unknown.
        assert_eq!(classify(&empty_report(0, 0), 0), ScrapeVerdict::Unknown);
        // Threshold gates on the actual estimate: below is live, equal is dead.
        let r = live_report(&["9.9.9.9", "8.8.8.8"]);
        let est = r.seeders_est().expect("two seeds estimate");
        assert!(est > 0, "{est}");
        let as_u32 = u32::try_from(est).unwrap();
        assert_eq!(
            classify(&r, est.saturating_sub(1)),
            ScrapeVerdict::Live { est: as_u32 }
        );
        assert_eq!(classify(&r, est), ScrapeVerdict::Dead { est: as_u32 });
    }
}
