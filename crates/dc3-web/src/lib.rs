//! Web front end for dhtcrawler3 (`docs/03-design.md` §12; R9–R12).
//!
//! * [`router`] builds the axum application: the pages, the JSON API and the
//!   security middleware (headers, rate limits, body, size and time limits).
//! * [`serve`] binds the listener, serves the application with graceful
//!   shutdown, and keeps the search index and the home-page statistics fresh.
//! * [`Backend`] is the database access the web role needs. It is
//!   implemented for [`dc3_store::Store`] and uses only the operations the
//!   `dc3_web` database user is granted.
//!
//! Pages are askama templates with HTML auto-escaping and contain no
//! JavaScript. JSON is built with `serde_json` from this crate's own types.
//!
//! Nothing here logs a visitor's address or search text (R11). Each request
//! is logged at `info` under the target `dc3_web::access` with its method,
//! route template, status and latency only. Metrics are emitted through the
//! `metrics` crate; the binary installs the recorder (names in
//! [`metric_names`]).
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]

mod app;
mod backend;
mod client_ip;
mod csrf;
mod format;
mod handlers;
mod json;
mod listener;
mod middleware;
mod ratelimit;
mod render;
mod serve;
mod static_files;
mod telemetry;
mod templates;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use dc3_policy::TermMatcher;
use dc3_search::SearchHandle;
use ipnet::IpNet;

pub use app::router;
pub use backend::{Backend, BackendError, BoxError};
pub use client_ip::{MAX_FORWARDED_ENTRIES, client_ip};
pub use listener::{CONNECTION_IDLE_TIMEOUT, HEADER_READ_TIMEOUT, MAX_CONNECTIONS};
pub use middleware::{
    CONTENT_SECURITY_POLICY, HSTS_VALUE, MAX_BODY_BYTES, MAX_CONCURRENT_REQUESTS,
    MAX_REPORT_BODY_BYTES, MAX_URI_BYTES, REQUEST_TIMEOUT,
};
pub use ratelimit::{
    API_QUOTA, OVERFLOW_SHARDS, PAGE_QUOTA, Quota, RATE_LIMIT_IDLE_EXPIRY, RATE_LIMIT_MAX_KEYS,
    REPORT_QUOTA, V6_QUOTA_MULTIPLIERS,
};
pub use serve::{
    SEARCH_WATCH_INTERVAL, SHUTDOWN_GRACE, STATS_LOAD_TIMEOUT, STATS_REFRESH_INTERVAL,
    STATS_RETRY_INTERVAL, serve,
};
pub use telemetry::{describe_metrics, metric_names};

/// Most torrent-detail lookups (page or API) handled at once. Each one holds
/// a full stored file list in memory.
pub const MAX_CONCURRENT_DETAILS: usize = 16;
/// A detail page or API response lists file paths until they add up to this
/// many characters; the rest is left out and the list is marked truncated.
pub const MAX_LISTED_PATH_CHARS: usize = 128_000;
/// A detail page or API response matches listed file paths against the
/// blocked terms only within this many bytes of paths (the name is always
/// matched). It bounds the matcher's work per request.
pub const MAX_CHECKED_PATH_BYTES: usize = 8 * 1024;
/// How long `/readyz` waits for the database.
pub const READY_TIMEOUT: Duration = Duration::from_secs(2);
/// `Expires` in `security.txt` lies this far after process start.
pub const SECURITY_TXT_VALIDITY: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Settings of the web role (the `[web]` configuration section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Public origin without a trailing slash, e.g. `https://search.example.org`.
    /// Used for the CSRF `Origin` check and in `security.txt`.
    pub base_url: String,
    /// Shown in page titles and headers.
    pub site_name: String,
    /// Shown on `/legal` and in `security.txt`; may be empty.
    pub contact_email: String,
    /// DMCA designated agent (free text) shown on `/legal`; may be empty.
    pub dmca_agent: String,
    /// Send `Strict-Transport-Security`.
    pub hsts: bool,
    /// Reverse proxies whose `X-Forwarded-For` header is trusted.
    pub trusted_proxies: Vec<IpNet>,
}

impl WebConfig {
    /// Checks the values [`serve`] relies on: `base_url` must be an
    /// `http://` or `https://` origin, and `site_name` must not be empty.
    pub fn validate(&self) -> Result<(), WebError> {
        app::validate_base_url(&self.base_url).map_err(WebError::Config)?;
        if self.site_name.trim().is_empty() {
            return Err(WebError::Config("web.site_name must not be empty".into()));
        }
        if self.site_name.chars().any(char::is_control) {
            return Err(WebError::Config(
                "web.site_name must not contain control characters".into(),
            ));
        }
        if self
            .contact_email
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(WebError::Config(
                "web.contact_email must not contain spaces or control characters".into(),
            ));
        }
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
    /// Blocked search terms (R18).
    pub policy: Arc<TermMatcher>,
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
