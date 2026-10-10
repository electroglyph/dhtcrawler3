//! Web front end for dhtcrawler4 (`docs/03-design.md` §12; R9–R12).
//!
//! * [`router`] builds the axum application: the pages, the JSON API and the
//!   security middleware (headers, rate limits, body, size and time limits).
//! * [`serve`] binds the listener, serves the application with graceful
//!   shutdown, and keeps the search index and the home-page statistics fresh.
//! * [`Backend`] is the database access the web role needs. It is
//!   implemented for [`dc4_store::Store`] and uses only the operations the
//!   `dc4_web` database user is granted.
//!
//! Pages are askama templates with HTML auto-escaping and contain no
//! JavaScript. The only cookie is the optional `theme` preference stored by
//! `POST /theme` when the visitor uses the dark/light mode switch. JSON is
//! built with `serde_json` from this crate's own types.
//!
//! Nothing here logs a visitor's address or search text (R11). Each request
//! is logged at `info` under the target `dc4_web::access` with its method,
//! route template, status and latency only. Metrics are emitted through the
//! `metrics` crate; the binary installs the recorder (names in
//! [`metric_names`]).
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]

mod app;
mod backend;
mod client_ip;
mod format;
mod handlers;
mod json;
mod listener;
mod middleware;
mod ratelimit;
mod render;
mod search_cache;
mod serve;
mod static_files;
mod telemetry;
mod templates;
mod theme;

use std::net::SocketAddr;
use std::time::Duration;

use dc4_search::SearchHandle;
use ipnet::IpNet;

pub use app::router;
pub use backend::{Backend, BackendError, BoxError};
pub use client_ip::{MAX_FORWARDED_ENTRIES, client_ip};
pub use listener::{CONNECTION_IDLE_TIMEOUT, HEADER_READ_TIMEOUT, MAX_CONNECTIONS};
pub use middleware::{
    CONTENT_SECURITY_POLICY, HSTS_VALUE, MAX_BODY_BYTES, MAX_CONCURRENT_REQUESTS, MAX_URI_BYTES,
    REQUEST_TIMEOUT,
};
pub use ratelimit::{
    API_QUOTA, OVERFLOW_SHARDS, PAGE_QUOTA, Quota, RATE_LIMIT_IDLE_EXPIRY, RATE_LIMIT_MAX_KEYS,
    V6_QUOTA_MULTIPLIERS,
};
pub use serve::{
    SEARCH_WATCH_INTERVAL, SHUTDOWN_GRACE, STATS_LOAD_TIMEOUT, STATS_REFRESH_INTERVAL,
    STATS_RETRY_INTERVAL, serve,
};
pub use telemetry::{describe_metrics, metric_names};

pub use search_cache::{
    DEFAULT_SEARCH_CACHE_ENTRIES, DEFAULT_SEARCH_CACHE_TTL_SECS, MAX_SEARCH_CACHE_ENTRIES,
    MAX_SEARCH_CACHE_TTL_SECS, SearchCacheConfig, validate_search_cache,
};

/// Most torrent-detail lookups (page or API) handled at once. Each one holds
/// a full stored file list in memory.
pub const MAX_CONCURRENT_DETAILS: usize = 16;
/// A detail page or API response lists file paths until they add up to this
/// many characters; the rest is left out and the list is marked truncated.
pub const MAX_LISTED_PATH_CHARS: usize = 128_000;
/// How long `/readyz` waits for the database.
pub const READY_TIMEOUT: Duration = Duration::from_secs(2);

/// Settings of the web role (the `[web]` configuration section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Shown in page titles and headers.
    pub site_name: String,
    /// Send `Strict-Transport-Security`.
    pub hsts: bool,
    /// Reverse proxies whose `X-Forwarded-For` header is trusted.
    pub trusted_proxies: Vec<IpNet>,
    /// How long a BEP 33 seeder estimate is shown and used for ranking
    /// after its scrape. Older estimates are hidden (stale) and rank as
    /// if missing. The crawl role sets this to `scrape_interval_secs`.
    pub seeder_freshness: Duration,
    /// How many `(text, sort, page, per_page)` index results to cache.
    /// Zero disables the search query cache.
    pub search_cache_entries: usize,
    /// How long a search cache entry lives (fixed from insert).
    /// Zero disables the search query cache.
    pub search_cache_ttl: Duration,
}

impl WebConfig {
    /// Checks the values [`serve`] relies on: `site_name` must not be empty.
    pub fn validate(&self) -> Result<(), WebError> {
        if self.site_name.trim().is_empty() {
            return Err(WebError::Config("web.site_name must not be empty".into()));
        }
        if self.site_name.chars().any(char::is_control) {
            return Err(WebError::Config(
                "web.site_name must not contain control characters".into(),
            ));
        }
        validate_search_cache(self.search_cache_entries, self.search_cache_ttl.as_secs())
            .map_err(WebError::Config)?;
        Ok(())
    }
}

/// What the web role works with besides its configuration.
#[derive(Clone)]
pub struct WebDeps<B: Backend> {
    /// Database access.
    pub backend: B,
    /// The live search index.
    pub search: SearchHandle,
}

/// Errors from [`serve`] and [`WebConfig::validate`].
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// A configuration value is unusable.
    #[error("invalid web configuration: {0}")]
    Config(String),
    /// The listen address could not be bound.
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    /// The server stopped with an I/O error.
    #[error("web server failed: {0}")]
    Serve(#[source] std::io::Error),
}
