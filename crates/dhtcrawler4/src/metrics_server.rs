//! The internal metrics listener (R19): `/metrics`, `/healthz` and
//! `/readyz` on `metrics.listen`. It serves operators and container
//! health checks only, never the public.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How often histogram and summary data is drained.
pub const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);
/// Content type of the Prometheus text format.
pub const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
/// Histogram buckets (seconds) for `dc4_search_seconds`.
pub const SEARCH_SECONDS_BUCKETS: [f64; 11] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
];
const SEARCH_SECONDS_METRIC: &str = "dc4_search_seconds";

/// Why the metrics listener could not start.
#[derive(Debug, thiserror::Error)]
pub enum MetricsError {
    #[error("cannot install the metrics recorder: {0}")]
    Recorder(String),
    #[error("cannot listen for metrics on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("metrics listener error: {0}")]
    Io(#[from] std::io::Error),
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

static RECORDER: Mutex<Option<PrometheusHandle>> = Mutex::new(None);

/// Installs the process-wide Prometheus recorder, once; later calls return
/// the same handle.
pub fn install_recorder() -> Result<PrometheusHandle, MetricsError> {
    let mut slot = lock(&RECORDER);
    if let Some(handle) = slot.as_ref() {
        return Ok(handle.clone());
    }
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(SEARCH_SECONDS_METRIC.to_owned()),
            &SEARCH_SECONDS_BUCKETS,
        )
        .map_err(|e| MetricsError::Recorder(e.to_string()))?
        .install_recorder()
        .map_err(|e| MetricsError::Recorder(e.to_string()))?;
    *slot = Some(handle.clone());
    Ok(handle)
}

/// Readiness flags of the roles in this process. `/readyz` is 200 when
/// every registered flag is set, 503 otherwise (or when none is).
#[derive(Debug, Clone, Default)]
pub struct Readiness {
    flags: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
}

impl Readiness {
    /// No flags yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a flag, initially not ready.
    pub fn register(&self) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        lock(&self.flags).push(Arc::clone(&flag));
        flag
    }

    /// Whether every flag is set.
    pub fn is_ready(&self) -> bool {
        let flags = lock(&self.flags);
        !flags.is_empty() && flags.iter().all(|f| f.load(Ordering::Relaxed))
    }
}

#[derive(Clone)]
struct AppState {
    handle: PrometheusHandle,
    readiness: Readiness,
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, METRICS_CONTENT_TYPE)],
        state.handle.render(),
    )
}

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    if state.readiness.is_ready() {
        (StatusCode::OK, "ready\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
    }
}

/// The listener's routes.
pub fn router(handle: PrometheusHandle, readiness: Readiness) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(AppState { handle, readiness })
}

/// A running metrics listener.
#[derive(Debug)]
pub struct MetricsServer {
    local_addr: SocketAddr,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl MetricsServer {
    /// Binds `listen` and serves in the background.
    pub async fn start(
        listen: SocketAddr,
        handle: PrometheusHandle,
        readiness: Readiness,
    ) -> Result<Self, MetricsError> {
        let listener = TcpListener::bind(listen)
            .await
            .map_err(|source| MetricsError::Bind {
                addr: listen,
                source,
            })?;
        let local_addr = listener.local_addr()?;
        let stop = CancellationToken::new();
        let app = router(handle.clone(), readiness);
        let serve_stop = stop.clone();
        let task = tokio::spawn(async move {
            let upkeep = async {
                let mut ticker = tokio::time::interval(UPKEEP_INTERVAL);
                loop {
                    ticker.tick().await;
                    handle.run_upkeep();
                }
            };
            let server =
                axum::serve(listener, app).with_graceful_shutdown(serve_stop.cancelled_owned());
            tokio::select! {
                result = server => {
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "the metrics listener failed");
                    }
                }
                () = upkeep => {}
            }
        });
        tracing::info!(port = local_addr.port(), "metrics listener started");
        Ok(Self {
            local_addr,
            stop,
            task,
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops serving and waits for the listener to close.
    pub async fn stop(self) {
        self.stop.cancel();
        if let Err(e) = self.task.await {
            tracing::warn!(error = %e, "the metrics listener task failed");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::healthcheck;

    #[tokio::test]
    async fn serves_metrics_health_and_readiness() {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let readiness = Readiness::new();
        let server =
            MetricsServer::start("127.0.0.1:0".parse().unwrap(), handle, readiness.clone())
                .await
                .unwrap();
        let base = format!("http://{}", server.local_addr());
        assert!(healthcheck::check(&format!("{base}/healthz")).await.is_ok());
        assert!(healthcheck::check(&format!("{base}/metrics")).await.is_ok());
        // No flags registered: not ready.
        assert!(healthcheck::check(&format!("{base}/readyz")).await.is_err());
        let a = readiness.register();
        let b = readiness.register();
        a.store(true, Ordering::Relaxed);
        assert!(healthcheck::check(&format!("{base}/readyz")).await.is_err());
        b.store(true, Ordering::Relaxed);
        assert!(healthcheck::check(&format!("{base}/readyz")).await.is_ok());
        assert!(healthcheck::check(&format!("{base}/nope")).await.is_err());
        server.stop().await;
    }

    #[test]
    fn recorder_renders_pipeline_metrics() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("dc4_fetch_total", "outcome" => "ok").increment(2);
            metrics::gauge!("dc4_queue_depth").set(7.0);
        });
        let text = handle.render();
        assert!(text.contains("dc4_fetch_total{outcome=\"ok\"} 2"), "{text}");
        assert!(text.contains("dc4_queue_depth 7"), "{text}");
    }
}
