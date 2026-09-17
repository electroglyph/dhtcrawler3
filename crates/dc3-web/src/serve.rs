//! Running the web role: listener, graceful shutdown and background tasks.

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use axum::serve::ListenerExt;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::app::{self, AppState};
use crate::listener::{CONNECTION_IDLE_TIMEOUT, GuardedListener, MAX_CONNECTIONS, set_nodelay};
use crate::telemetry::describe_metrics;
use crate::{Backend, WebConfig, WebDeps, WebError};

/// How often the search index is checked for a new generation or commit.
pub const SEARCH_WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// How often the home-page statistics are reloaded.
pub const STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// Retry interval while no statistics have been loaded yet.
pub const STATS_RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// Longest wait for one statistics query.
pub const STATS_LOAD_TIMEOUT: Duration = Duration::from_secs(5);
/// After shutdown starts, open connections get this long to finish.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);
/// Longest wait for a background task to stop.
const TASK_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Serves the web role on `cfg.listen` until `shutdown` completes.
///
/// Before accepting connections it checks the configuration, registers the
/// metric descriptions and loads the home-page statistics once. While
/// running it reloads the statistics every [`STATS_REFRESH_INTERVAL`]
/// (keeping the previous values when that fails) and follows the search
/// index every [`SEARCH_WATCH_INTERVAL`]. When `shutdown` completes it stops
/// accepting connections, lets open ones finish for up to
/// [`SHUTDOWN_GRACE`], and stops the background tasks.
pub async fn serve<B: Backend>(
    cfg: WebConfig,
    deps: WebDeps<B>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), WebError> {
    cfg.validate()?;
    describe_metrics();
    let listen = cfg.listen;
    let search = deps.search.clone();
    let (router, state) = app::build(cfg, deps);

    let tcp = TcpListener::bind(listen)
        .await
        .map_err(|source| WebError::Bind {
            addr: listen,
            source,
        })?;
    let local = tcp.local_addr().unwrap_or(listen);
    let listener =
        GuardedListener::new(tcp, MAX_CONNECTIONS, CONNECTION_IDLE_TIMEOUT).tap_io(set_nodelay);

    let stats_failing = refresh_stats(&state, false).await;

    let (stop_tx, stop_rx) = watch::channel(false);
    let stop_tx = Arc::new(stop_tx);
    let forwarder = {
        let stop_tx = Arc::clone(&stop_tx);
        tokio::spawn(async move {
            shutdown.await;
            stop_tx.send_replace(true);
        })
    };
    let watcher = tokio::spawn(search.watch(SEARCH_WATCH_INTERVAL, stopped(stop_rx.clone())));
    let refresher = {
        let state = Arc::clone(&state);
        let stop = stop_rx.clone();
        tokio::spawn(async move { stats_loop(&state, stop, stats_failing).await })
    };

    tracing::info!(addr = %local, "web server listening");
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stopped(stop_rx.clone()));
    let result = until_drained(server.into_future(), stop_rx).await;

    stop_tx.send_replace(true);
    forwarder.abort();
    stop_task(watcher, "search index watch").await;
    stop_task(refresher, "statistics refresh").await;
    tracing::info!("web server stopped");
    result.map_err(WebError::Serve)
}

/// Completes when a stop is signalled (or the signal can no longer come).
async fn stopped(mut stop: watch::Receiver<bool>) {
    // An error means the sender is gone, which also means stop.
    let _ = stop.wait_for(|stop| *stop).await;
}

/// Runs the server until it has drained, or until [`SHUTDOWN_GRACE`] after a
/// stop was signalled.
async fn until_drained(
    server: impl Future<Output = io::Result<()>>,
    stop: watch::Receiver<bool>,
) -> io::Result<()> {
    let server = pin!(server);
    let deadline = async {
        stopped(stop).await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    tokio::select! {
        result = server => result,
        () = deadline => {
            tracing::warn!("connections still open after the shutdown grace period; closing them");
            Ok(())
        }
    }
}

/// Waits for a background task to finish, aborting it after
/// [`TASK_STOP_TIMEOUT`].
async fn stop_task(task: JoinHandle<()>, name: &'static str) {
    let abort = task.abort_handle();
    match tokio::time::timeout(TASK_STOP_TIMEOUT, task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(task = name, error = %e, "background task failed"),
        Err(_) => {
            abort.abort();
            tracing::warn!(task = name, "background task did not stop in time; aborted");
        }
    }
}

/// Reloads the home-page statistics every [`STATS_REFRESH_INTERVAL`]
/// (every [`STATS_RETRY_INTERVAL`] until the first success) until stopped.
async fn stats_loop<B: Backend>(state: &AppState<B>, stop: watch::Receiver<bool>, failing: bool) {
    let mut failing = failing;
    let mut stop = pin!(stopped(stop));
    loop {
        let wait = if state.stats().is_some() {
            STATS_REFRESH_INTERVAL
        } else {
            STATS_RETRY_INTERVAL
        };
        tokio::select! {
            () = &mut stop => break,
            () = tokio::time::sleep(wait) => {}
        }
        tokio::select! {
            () = &mut stop => break,
            now_failing = refresh_stats(state, failing) => failing = now_failing,
        }
    }
}

/// Loads the statistics once. Returns whether the load failed; failures are
/// logged only when the state changes. The previous values stay in place
/// on failure.
pub(crate) async fn refresh_stats<B: Backend>(state: &AppState<B>, was_failing: bool) -> bool {
    let error = match tokio::time::timeout(STATS_LOAD_TIMEOUT, state.backend.public_stats()).await {
        Ok(Ok(stats)) => {
            state.set_stats(stats);
            None
        }
        Ok(Err(e)) => Some(e.to_string()),
        Err(_) => Some("timed out".to_owned()),
    };
    match (&error, was_failing) {
        (Some(e), false) => tracing::warn!(
            error = %e,
            "cannot load the home-page statistics; keeping the previous values"
        ),
        (None, true) => tracing::info!("home-page statistics loaded again"),
        _ => {}
    }
    error.is_some()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arithmetic_side_effects)]

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use dc3_core::AnyKey;
    use dc3_policy::TermMatcher;
    use dc3_search::SearchHandle;
    use dc3_store::{NewReport, PublicStats, SubmitOutcome, TorrentRecord};

    use super::*;
    use crate::BackendError;

    /// A backend whose statistics can be switched between values and
    /// failure, and which counts statistics calls.
    #[derive(Clone, Default)]
    struct StatsBackend(Arc<(Mutex<Option<PublicStats>>, AtomicUsize)>);

    impl StatsBackend {
        fn set(&self, stats: Option<PublicStats>) {
            *self.0.0.lock().unwrap() = stats;
        }

        fn calls(&self) -> usize {
            self.0.1.load(Ordering::SeqCst)
        }
    }

    impl Backend for StatsBackend {
        async fn get_by_key(&self, _: AnyKey) -> Result<Option<TorrentRecord>, BackendError> {
            Ok(None)
        }

        async fn get_many(&self, _: &[i64]) -> Result<Vec<TorrentRecord>, BackendError> {
            Ok(Vec::new())
        }

        async fn submit_report(&self, _: &NewReport) -> Result<SubmitOutcome, BackendError> {
            Err(BackendError::ReportsFull)
        }

        async fn public_stats(&self) -> Result<PublicStats, BackendError> {
            self.0.1.fetch_add(1, Ordering::SeqCst);
            (*self.0.0.lock().unwrap()).ok_or_else(|| BackendError::Unavailable("down".into()))
        }

        async fn ping(&self) -> Result<(), BackendError> {
            Ok(())
        }
    }

    fn stats(torrents: i64) -> PublicStats {
        PublicStats {
            torrents,
            added_today: 1,
            added_yesterday: 2,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn statistics_are_retried_refreshed_and_kept_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StatsBackend::default();
        let cfg = WebConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "https://s.example".into(),
            site_name: "s".into(),
            contact_email: String::new(),
            dmca_agent: String::new(),
            hsts: false,
            trusted_proxies: Vec::new(),
        };
        let deps = WebDeps {
            backend: backend.clone(),
            search: SearchHandle::open(dir.path()).unwrap(),
            policy: Arc::new(TermMatcher::empty()),
        };
        let (_router, state) = app::build(cfg, deps);

        // The startup load fails: nothing is shown yet.
        assert!(refresh_stats(&state, false).await);
        assert_eq!(state.stats(), None);
        assert_eq!(backend.calls(), 1);

        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn({
            let state = Arc::clone(&state);
            async move { stats_loop(&state, stop_rx, true).await }
        });
        let tick = Duration::from_millis(1);

        // Retried after the short interval, still failing.
        tokio::time::sleep(STATS_RETRY_INTERVAL + tick).await;
        assert_eq!(backend.calls(), 2);
        assert_eq!(state.stats(), None);

        // The next retry succeeds.
        backend.set(Some(stats(10)));
        tokio::time::sleep(STATS_RETRY_INTERVAL).await;
        assert_eq!(backend.calls(), 3);
        assert_eq!(state.stats(), Some(stats(10)));

        // From now on the long interval applies.
        backend.set(Some(stats(20)));
        tokio::time::sleep(STATS_RETRY_INTERVAL).await;
        assert_eq!(backend.calls(), 3);
        tokio::time::sleep(STATS_REFRESH_INTERVAL).await;
        assert_eq!(backend.calls(), 4);
        assert_eq!(state.stats(), Some(stats(20)));

        // A failure keeps the previous values.
        backend.set(None);
        tokio::time::sleep(STATS_REFRESH_INTERVAL).await;
        assert_eq!(backend.calls(), 5);
        assert_eq!(state.stats(), Some(stats(20)));

        // The loop ends when told to stop.
        stop_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(backend.calls(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_statistics_time_out() {
        #[derive(Clone)]
        struct Slow;
        impl Backend for Slow {
            async fn get_by_key(&self, _: AnyKey) -> Result<Option<TorrentRecord>, BackendError> {
                Ok(None)
            }
            async fn get_many(&self, _: &[i64]) -> Result<Vec<TorrentRecord>, BackendError> {
                Ok(Vec::new())
            }
            async fn submit_report(&self, _: &NewReport) -> Result<SubmitOutcome, BackendError> {
                Err(BackendError::ReportsFull)
            }
            async fn public_stats(&self) -> Result<PublicStats, BackendError> {
                std::future::pending().await
            }
            async fn ping(&self) -> Result<(), BackendError> {
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = WebConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            base_url: "https://s.example".into(),
            site_name: "s".into(),
            contact_email: String::new(),
            dmca_agent: String::new(),
            hsts: false,
            trusted_proxies: Vec::new(),
        };
        let deps = WebDeps {
            backend: Slow,
            search: SearchHandle::open(dir.path()).unwrap(),
            policy: Arc::new(TermMatcher::empty()),
        };
        let (_router, state) = app::build(cfg, deps);
        let started = tokio::time::Instant::now();
        assert!(refresh_stats(&state, true).await);
        assert!(started.elapsed() >= STATS_LOAD_TIMEOUT);
        assert!(started.elapsed() < STATS_LOAD_TIMEOUT + Duration::from_secs(1));
        assert_eq!(state.stats(), None);
    }
}
