//! Application state and the router.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, PoisonError, RwLock};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use chrono::{DateTime, TimeDelta, Utc};
use dc3_policy::TermMatcher;
use dc3_search::{MAX_QUERY_CHARS, SearchHandle};
use dc3_store::PublicStats;
use ipnet::IpNet;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::sync::Semaphore;

use crate::handlers::{api, health, pages, report, search, torrent};
use crate::middleware::{MAX_BODY_BYTES, MAX_CONCURRENT_REQUESTS, MAX_REPORT_BODY_BYTES, guard};
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
    pub const REPORT: &str = "/report/{key}";
    pub const API_SEARCH: &str = "/api/v1/search";
    pub const API_TORRENT: &str = "/api/v1/torrents/{key}";
    pub const ABOUT: &str = "/about";
    pub const LEGAL: &str = "/legal";
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
    pub const ALL: [&str; 14] = [
        HOME,
        SEARCH,
        TORRENT,
        REPORT,
        API_SEARCH,
        API_TORRENT,
        ABOUT,
        LEGAL,
        PRIVACY,
        ROBOTS,
        SECURITY_TXT,
        STYLE,
        HEALTHZ,
        READYZ,
    ];

    /// Prefix of torrent page paths.
    pub const TORRENT_PREFIX: &str = "/t/";
    /// Prefix of report form paths.
    pub const REPORT_PREFIX: &str = "/report/";
    /// Prefix of every JSON API path.
    pub const API_PREFIX: &str = "/api/";
}

/// Characters left as they are in a `mailto:` address.
const MAILTO_KEEP: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'@')
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'+')
    .remove(b'~');

/// Site name used when the configured one is blank.
const DEFAULT_SITE_NAME: &str = "dhtcrawler3";

/// The configuration, cleaned up for use in pages and headers.
pub(crate) struct Site {
    /// Public origin without a trailing slash.
    pub base_url: String,
    pub site_name: String,
    pub contact_email: String,
    pub dmca_agent: String,
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
        let contact_email = single_line(&cfg.contact_email);
        let dmca_agent = cfg
            .dmca_agent
            .chars()
            .filter(|c| *c == '\n' || !c.is_control())
            .collect::<String>()
            .trim()
            .to_owned();
        let security_txt = security_txt(&base_url, &contact_email, started);
        Site {
            base_url,
            site_name,
            contact_email,
            dmca_agent,
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

    /// The contact address as a `mailto:` URI, if one is configured.
    pub(crate) fn mailto(&self) -> Option<String> {
        if self.contact_email.is_empty() {
            return None;
        }
        Some(format!(
            "mailto:{}",
            utf8_percent_encode(&self.contact_email, MAILTO_KEEP)
        ))
    }
}

/// Shared state of every request.
pub(crate) struct AppState<B> {
    pub site: Site,
    pub backend: B,
    pub search: SearchHandle,
    pub policy: Arc<TermMatcher>,
    /// Home-page totals; `None` until the first successful load.
    stats: RwLock<Option<PublicStats>>,
    pub limiter: RateLimiter,
    /// Global request concurrency.
    pub requests: Arc<Semaphore>,
    /// Concurrency of torrent-detail lookups.
    pub details: Semaphore,
    /// Set once the missing-peer-address misconfiguration has been logged.
    pub warned_no_peer: AtomicBool,
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
pub fn router<B: Backend>(cfg: WebConfig, deps: WebDeps<B>) -> Router {
    build(cfg, deps).0
}

/// The router and the state it shares with the background tasks.
pub(crate) fn build<B: Backend>(cfg: WebConfig, deps: WebDeps<B>) -> (Router, Arc<AppState<B>>) {
    let state = Arc::new(AppState {
        site: Site::new(cfg, Utc::now()),
        backend: deps.backend,
        search: deps.search,
        policy: deps.policy,
        stats: RwLock::new(None),
        limiter: RateLimiter::new(RATE_LIMIT_MAX_KEYS, RATE_LIMIT_IDLE_EXPIRY),
        requests: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
        details: Semaphore::new(MAX_CONCURRENT_DETAILS),
        warned_no_peer: AtomicBool::new(false),
    });

    let router = Router::new()
        .route(routes::HOME, get(pages::home::<B>))
        .route(routes::SEARCH, get(search::search_page::<B>))
        .route(routes::TORRENT, get(torrent::torrent_page::<B>))
        .route(
            routes::REPORT,
            get(report::report_form::<B>)
                .post(report::report_submit::<B>)
                .layer(DefaultBodyLimit::max(MAX_REPORT_BODY_BYTES)),
        )
        .route(routes::API_SEARCH, get(api::search::<B>))
        .route(routes::API_TORRENT, get(api::torrent::<B>))
        .route(routes::ABOUT, get(pages::about::<B>))
        .route(routes::LEGAL, get(pages::legal::<B>))
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
pub(crate) fn validate_base_url(url: &str) -> Result<(), String> {
    let url = url.trim_end_matches('/');
    let Some(authority) = strip_scheme(url) else {
        return Err(format!(
            "web.base_url must start with http:// or https://, got {url:?}"
        ));
    };
    if authority.is_empty() {
        return Err("web.base_url has no host".into());
    }
    let bad =
        |c: char| matches!(c, '/' | '?' | '#' | '@' | '\\') || c.is_whitespace() || c.is_control();
    if authority.chars().any(bad) {
        return Err(format!(
            "web.base_url must be an origin such as https://search.example.org (no path), got {url:?}"
        ));
    }
    if has_default_port(url, authority) {
        return Err(format!(
            "web.base_url must omit the default port, as browsers do (got {url:?})"
        ));
    }
    Ok(())
}

fn has_default_port(url: &str, authority: &str) -> bool {
    let default_port = if url.len() >= 8 && url[..8].eq_ignore_ascii_case("https://") {
        443
    } else {
        80
    };
    let port_str = if let Some(inner) = authority.strip_prefix('[') {
        let Some((_, after)) = inner.split_once(']') else {
            return false;
        };
        match after.strip_prefix(':') {
            Some(p) => p,
            None => return false,
        }
    } else {
        match authority.rsplit_once(':') {
            Some((_, p)) => p,
            None => return false,
        }
    };
    port_str.parse::<u16>().is_ok_and(|p| p == default_port)
}

fn strip_scheme(url: &str) -> Option<&str> {
    ["https://", "http://"].iter().find_map(|scheme| {
        let head = url.get(..scheme.len())?;
        head.eq_ignore_ascii_case(scheme)
            .then(|| url.get(scheme.len()..))
            .flatten()
    })
}

/// `s` trimmed, with control characters removed.
fn single_line(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The RFC 9116 security.txt body.
fn security_txt(base_url: &str, contact_email: &str, started: DateTime<Utc>) -> String {
    let contact = if contact_email.is_empty() {
        format!("{base_url}{}", routes::LEGAL)
    } else {
        format!("mailto:{}", utf8_percent_encode(contact_email, MAILTO_KEEP))
    };
    let validity = TimeDelta::from_std(SECURITY_TXT_VALIDITY).unwrap_or(TimeDelta::zero());
    let expires = started.checked_add_signed(validity).unwrap_or(started);
    format!(
        "Contact: {contact}\nExpires: {}\nCanonical: {base_url}{}\nPreferred-Languages: en\n",
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
            "https://search.example.org/",
            "HTTPS://Search.Example.org",
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
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn security_txt_with_and_without_contact() {
        let started = Utc.with_ymd_and_hms(2026, 9, 17, 10, 0, 0).unwrap();
        let with = security_txt("https://s.example", "sec+web@s.example", started);
        assert_eq!(
            with,
            "Contact: mailto:sec+web@s.example\n\
             Expires: 2027-09-17T10:00:00Z\n\
             Canonical: https://s.example/.well-known/security.txt\n\
             Preferred-Languages: en\n"
        );
        let without = security_txt("https://s.example", "", started);
        assert!(without.starts_with("Contact: https://s.example/legal\n"));
    }

    #[test]
    fn config_text_is_cleaned() {
        assert_eq!(single_line("  a\r\nb\u{7}  "), "ab");
        let site = Site::new(
            WebConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                base_url: "https://s.example/".into(),
                site_name: "  ".into(),
                contact_email: "a b@c".into(),
                dmca_agent: " Agent\nStreet 1\u{1b}[31m ".into(),
                hsts: false,
                trusted_proxies: Vec::new(),
            },
            Utc::now(),
        );
        assert_eq!(site.base_url, "https://s.example");
        assert_eq!(site.site_name, DEFAULT_SITE_NAME);
        assert_eq!(site.dmca_agent, "Agent\nStreet 1[31m");
        assert_eq!(site.mailto().as_deref(), Some("mailto:a%20b@c"));
        assert_eq!(routes::ALL.len(), 14);
    }
}
