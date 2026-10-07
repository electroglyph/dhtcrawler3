//! Application state and the router.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use chrono::{DateTime, TimeDelta, Utc};
use dc3_search::{MAX_QUERY_CHARS, SearchHandle};
use dc3_store::PublicStats;
use ipnet::IpNet;
use tokio::sync::Semaphore;

use crate::handlers::{api, health, pages, search, torrent};
use crate::middleware::{MAX_BODY_BYTES, MAX_CONCURRENT_REQUESTS, guard};
use crate::ratelimit::{RATE_LIMIT_IDLE_EXPIRY, RATE_LIMIT_MAX_KEYS, RateLimiter};
use crate::templates::PageMeta;
use crate::{
    Backend, MAX_CONCURRENT_DETAILS, SECURITY_TXT_VALIDITY, WebConfig, WebDeps, format,
    static_files,
};

/// Route templates. They double as the `route` metric label.
pub(crate) mod routes {
    pub const HOME: &str = "/";
    pub const SEARCH: &str = "/search";
    pub const TORRENT: &str = "/t/{key}";
    pub const API_SEARCH: &str = "/api/v1/search";
    pub const API_TORRENT: &str = "/api/v1/torrents/{key}";
    pub const ABOUT: &str = "/about";
    pub const PRIVACY: &str = "/privacy";
    pub const ROBOTS: &str = "/robots.txt";
    pub const SECURITY_TXT: &str = "/.well-known/security.txt";
    pub const STYLE: &str = "/static/style.css";
    pub const HEALTHZ: &str = "/healthz";
    pub const READYZ: &str = "/readyz";
    /// Label of the stylesheet path that carries its hash.
    pub const STYLE_HASHED: &str = "/static/style.{hash}.css";
    /// Label of requests that matched no route.
    pub const UNMATCHED: &str = "unmatched";

    /// Every fixed route template.
    pub const ALL: [&str; 12] = [
        HOME,
        SEARCH,
        TORRENT,
        API_SEARCH,
        API_TORRENT,
        ABOUT,
        PRIVACY,
        ROBOTS,
        SECURITY_TXT,
        STYLE,
        HEALTHZ,
        READYZ,
    ];

    /// Prefix of torrent page paths.
    pub const TORRENT_PREFIX: &str = "/t/";
    /// Prefix of every JSON API path.
    pub const API_PREFIX: &str = "/api/";
}

/// Site name used when the configured one is blank.
const DEFAULT_SITE_NAME: &str = "dhtcrawler3";

/// The configuration, cleaned up for use in pages and headers.
pub(crate) struct Site {
    pub site_name: String,
    pub hsts: bool,
    pub trusted_proxies: Vec<IpNet>,
    /// The body of `/.well-known/security.txt`.
    pub security_txt: String,
}

impl Site {
    fn new(cfg: WebConfig, started: DateTime<Utc>) -> Site {
        let base_url = single_line(&cfg.base_url).trim_end_matches('/').to_owned();
        let site_name = match single_line(&cfg.site_name) {
            name if name.is_empty() => DEFAULT_SITE_NAME.to_owned(),
            name => name,
        };
        let security_txt = security_txt(&base_url, started);
        Site {
            site_name,
            hsts: cfg.hsts,
            trusted_proxies: cfg.trusted_proxies,
            security_txt,
        }
    }

    /// Layout data for a page titled `title`; `query` prefills the header
    /// search box.
    pub(crate) fn page<'a>(
        &'a self,
        title: impl Into<String>,
        query: &'a str,
        header_search: bool,
    ) -> PageMeta<'a> {
        PageMeta {
            site_name: &self.site_name,
            style_path: static_files::style_path(),
            title: title.into(),
            query,
            header_search,
            max_query_chars: MAX_QUERY_CHARS,
        }
    }
}

/// Shared state of every request.
pub(crate) struct AppState<B> {
    pub site: Site,
    pub backend: B,
    pub search: SearchHandle,
    /// Home-page totals; `None` until the first successful load.
    stats: RwLock<Option<PublicStats>>,
    pub limiter: RateLimiter,
    /// Global request concurrency.
    pub requests: Arc<Semaphore>,
    /// Concurrency of torrent-detail lookups.
    pub details: Semaphore,
    /// Set once the missing-peer-address misconfiguration has been logged.
    pub warned_no_peer: AtomicBool,
    /// How long a BEP 33 seeder estimate counts as fresh (display + rank).
    pub seeder_freshness: Duration,
}

impl<B> AppState<B> {
    /// The current home-page totals.
    pub(crate) fn stats(&self) -> Option<PublicStats> {
        // Plain data behind the lock stays valid even after a panic.
        *self.stats.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replaces the home-page totals.
    pub(crate) fn set_stats(&self, stats: PublicStats) {
        *self.stats.write().unwrap_or_else(PoisonError::into_inner) = Some(stats);
    }
}

/// Builds the web application. It starts no background tasks: without
/// [`serve`](crate::serve) the search index is not reloaded and the home
/// page shows no statistics. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()`; requests without
/// a peer address are refused.
///
/// # Panics
///
/// Panics when `cfg` fails [`WebConfig::validate`]: a router built from an
/// unvalidated config would serve a malformed `security.txt` canonical URL.
/// [`serve`](crate::serve) validates first; direct callers must pass a
/// validated config.
pub fn router<B: Backend>(cfg: WebConfig, deps: WebDeps<B>) -> Router {
    build(cfg, deps).0
}

/// The router and the state it shares with the background tasks.
///
/// Panics on an invalid `cfg` (see [`router`]).
#[allow(clippy::panic)]
pub(crate) fn build<B: Backend>(cfg: WebConfig, deps: WebDeps<B>) -> (Router, Arc<AppState<B>>) {
    if let Err(e) = cfg.validate() {
        panic!("invalid WebConfig: {e}");
    }
    let seeder_freshness = cfg.seeder_freshness;
    let state = Arc::new(AppState {
        site: Site::new(cfg, Utc::now()),
        backend: deps.backend,
        search: deps.search,
        stats: RwLock::new(None),
        limiter: RateLimiter::new(RATE_LIMIT_MAX_KEYS, RATE_LIMIT_IDLE_EXPIRY),
        requests: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        details: Semaphore::new(MAX_CONCURRENT_DETAILS),
        warned_no_peer: AtomicBool::new(false),
        seeder_freshness,
    });

    let router = Router::new()
        .route(routes::HOME, get(pages::home::<B>))
        .route(routes::SEARCH, get(search::search_page::<B>))
        .route(routes::TORRENT, get(torrent::torrent_page::<B>))
        .route(routes::API_SEARCH, get(api::search::<B>))
        .route(routes::API_TORRENT, get(api::torrent::<B>))
        .route(routes::ABOUT, get(pages::about::<B>))
        .route(routes::PRIVACY, get(pages::privacy::<B>))
        .route(routes::ROBOTS, get(pages::robots))
        .route(routes::SECURITY_TXT, get(pages::security_txt::<B>))
        .route(routes::STYLE, get(pages::style))
        .route(static_files::style_path(), get(pages::style_immutable))
        .route(routes::HEALTHZ, get(health::healthz))
        .route(routes::READYZ, get(health::readyz::<B>))
        .fallback(pages::not_found::<B>)
        .method_not_allowed_fallback(pages::method_not_allowed::<B>)
        .with_state(Arc::clone(&state))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            guard::<B>,
        ));
    (router, state)
}

/// Checks that `url` is an `http://` or `https://` origin.
///
/// The check itself is shared with the binary config validator (see
/// `dc3_core::validate_base_url`); this only prefixes the field name so the
/// same value cannot pass here and fail there.
pub(crate) fn validate_base_url(url: &str) -> Result<(), String> {
    dc3_core::validate_base_url(url, dc3_core::MAX_BASE_URL_CHARS)
        .map_err(|reason| format!("web.base_url {reason}"))
}

/// `s` trimmed, with control characters removed.
fn single_line(s: &str) -> String {
    // Trim first so the filter only scans the kept middle; the trailing
    // truncate drops spaces a removed trailing control would expose
    // (`"foo \u{1}"` trims to `"foo"`, not `"foo "`).
    let mut out: String = s.trim().chars().filter(|c| !c.is_control()).collect();
    out.truncate(out.trim_end().len());
    out
}

/// The RFC 9116 security.txt body.
fn security_txt(base_url: &str, started: DateTime<Utc>) -> String {
    let validity = TimeDelta::from_std(SECURITY_TXT_VALIDITY).unwrap_or(TimeDelta::zero());
    let expires = started.checked_add_signed(validity).unwrap_or(started);
    format!(
        "Contact: {base_url}/\nExpires: {}\nCanonical: {base_url}{}\nPreferred-Languages: en\n",
        format::rfc3339(expires),
        routes::SECURITY_TXT
    )
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn base_url_validation() {
        for ok in [
            "https://search.example.org",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
            "https://example.com:8443",
            "http://example.com:8080",
        ] {
            assert!(validate_base_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "search.example.org",
            "ftp://example.org",
            "https://search.example.org/",
            "HTTPS://Search.Example.org",
            "https://",
            "https://example.org/search",
            "https://example.org?x",
            "https://user@example.org",
            "https://exa mple.org",
            "https://example.org\r\nX: y",
            "https://example.com:443",
            "http://example.com:80",
            "https://example.com:443/",
            "https://[::1]:443",
            "http://[::1]:80",
            "https://[::1",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn security_txt_body() {
        let started = Utc.with_ymd_and_hms(2026, 9, 17, 10, 0, 0).unwrap();
        let txt = security_txt("https://s.example", started);
        assert_eq!(
            txt,
            "Contact: https://s.example/\n\
             Expires: 2027-09-17T10:00:00Z\n\
             Canonical: https://s.example/.well-known/security.txt\n\
             Preferred-Languages: en\n"
        );
    }

    #[test]
    fn config_text_is_cleaned() {
        assert_eq!(single_line("  a\r\nb\u{7}  "), "ab");
        // A removed trailing control must not expose a kept space.
        assert_eq!(single_line("foo \u{1}"), "foo");
        assert_eq!(single_line("  \u{1}foo\u{2}  "), "foo");
        assert_eq!(single_line("a b"), "a b");
        assert_eq!(single_line("   "), "");
        assert_eq!(single_line(""), "");
        let site = Site::new(
            WebConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "https://s.example/".into(),
                site_name: "  ".into(),
                hsts: false,
                trusted_proxies: Vec::new(),
                seeder_freshness: Duration::from_secs(604800),
            },
            Utc::now(),
        );
        assert!(site.security_txt.contains("Canonical: https://s.example/"));
        assert_eq!(site.site_name, DEFAULT_SITE_NAME);
        assert_eq!(routes::ALL.len(), 12);
    }
}
