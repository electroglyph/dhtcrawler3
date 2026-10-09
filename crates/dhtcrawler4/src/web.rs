//! The web role (design §12): serves `dc3-web` over the live search index.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dc3_search::{SearchError, SearchHandle};
use dc3_store::Store;
use dc3_web::{WebConfig, WebDeps};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ConfigError};

/// How often the web role checks the database for readiness.
pub const READINESS_INTERVAL: Duration = Duration::from_secs(5);
/// Time allowed for the readiness database ping.
pub const READINESS_PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the web role failed.
#[derive(Debug, thiserror::Error)]
pub enum WebRoleError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot open the search index: {0}")]
    Search(#[from] SearchError),
    #[error("web server error: {0}")]
    Web(#[from] dc3_web::WebError),
    #[error("web task failed: {0}")]
    Task(String),
}

/// The `dc3-web` configuration from `[web]`.
pub fn web_config(cfg: &Config) -> Result<WebConfig, ConfigError> {
    let w = &cfg.web;
    Ok(WebConfig {
        listen: w.listen,
        site_name: w.site_name.clone(),
        hsts: w.hsts,
        trusted_proxies: cfg.trusted_proxies()?,
        seeder_freshness: Duration::from_secs(cfg.crawl.scrape_interval_secs),
        search_cache_entries: w.search_cache_size,
        search_cache_ttl: Duration::from_secs(w.search_cache_ttl_secs),
    })
}

/// Opens the live search index generation under `path` (creating the index
/// root if it is new), off the async threads.
pub async fn open_search(path: &Path) -> Result<SearchHandle, WebRoleError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || SearchHandle::open(&path))
        .await
        .map_err(|e| WebRoleError::Task(e.to_string()))?
        .map_err(WebRoleError::Search)
}

/// Serves the site until `cancel` is cancelled. `ready` is true while the
/// database answers (the index is open by construction).
pub async fn run(
    cfg: WebConfig,
    store: Store,
    search: SearchHandle,
    ready: Arc<AtomicBool>,
    cancel: CancellationToken,
) -> Result<(), WebRoleError> {
    let stop = cancel.child_token();
    let readiness = tokio::spawn(track_database(
        store.clone(),
        Arc::clone(&ready),
        stop.clone(),
    ));
    let deps = WebDeps {
        backend: store,
        search,
    };
    let result = dc3_web::serve(cfg, deps, cancel.cancelled_owned()).await;
    stop.cancel();
    if let Err(e) = readiness.await {
        tracing::warn!(error = %e, "the web readiness task failed");
    }
    ready.store(false, Ordering::Relaxed);
    result.map_err(WebRoleError::Web)
}

async fn track_database(store: Store, ready: Arc<AtomicBool>, stop: CancellationToken) {
    loop {
        let ok = matches!(
            tokio::time::timeout(READINESS_PING_TIMEOUT, store.ping()).await,
            Ok(Ok(()))
        );
        ready.store(ok && !stop.is_cancelled(), Ordering::Relaxed);
        tokio::select! {
            () = stop.cancelled() => break,
            () = tokio::time::sleep(READINESS_INTERVAL) => {}
        }
    }
    ready.store(false, Ordering::Relaxed);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn config_mapping() {
        let mut cfg = Config::default();
        cfg.web.trusted_proxies = vec!["172.30.80.0/24".into()];
        cfg.web.hsts = true;
        let w = web_config(&cfg).unwrap();
        assert_eq!(w.listen, cfg.web.listen);
        assert_eq!(w.site_name, "dhtcrawler4");
        assert!(w.hsts);
        assert_eq!(
            w.search_cache_entries,
            dc3_web::DEFAULT_SEARCH_CACHE_ENTRIES
        );
        assert_eq!(
            w.search_cache_ttl,
            Duration::from_secs(dc3_web::DEFAULT_SEARCH_CACHE_TTL_SECS)
        );
        assert_eq!(
            w.trusted_proxies,
            vec!["172.30.80.0/24".parse::<ipnet::IpNet>().unwrap()]
        );
    }
}
