//! The middleware around every request (`docs/03-design.md` §12).
//!
//! In order: address length, declared body size, rate limit, global
//! concurrency and a time limit. Every response, including the ones made
//! here, then gets the security headers and a `Cache-Control`, is counted,
//! and is logged with its method, route template, status and latency only.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::connect_info::{ConnectInfo, MockConnectInfo};
use axum::extract::{MatchedPath, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY as CSP, REFERRER_POLICY, RETRY_AFTER,
    SERVER, STRICT_TRANSPORT_SECURITY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::app::{AppState, routes};
use crate::client_ip::client_ip;
use crate::ratelimit::{RateClass, Verdict};
use crate::render::{Flavor, NO_STORE, error_response};
use crate::telemetry::metric_names;
use crate::{Backend, static_files};

/// The Content-Security-Policy of every response.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'self'; \
img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
/// The Strict-Transport-Security value sent when `web.hsts` is set.
pub const HSTS_VALUE: &str = "max-age=31536000; includeSubDomains";
/// Longest time a request may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest request body.
pub const MAX_BODY_BYTES: usize = 4 * 1024;
/// Longest path and query string.
pub const MAX_URI_BYTES: usize = 4 * 1024;
/// Most requests handled at once; more get 503.
pub const MAX_CONCURRENT_REQUESTS: usize = 512;

/// Log target of the per-request log line.
const ACCESS_LOG_TARGET: &str = "dc3_web::access";
/// `Retry-After` of a 503 caused by overload, in seconds.
const OVERLOAD_RETRY_SECS: u64 = 1;

const PERMISSIONS_POLICY: HeaderName = HeaderName::from_static("permissions-policy");
const CROSS_ORIGIN_OPENER_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-opener-policy");
const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

const CSP_VALUE: HeaderValue = HeaderValue::from_static(CONTENT_SECURITY_POLICY);
const HSTS: HeaderValue = HeaderValue::from_static(HSTS_VALUE);
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const NO_REFERRER: HeaderValue = HeaderValue::from_static("no-referrer");
const PERMISSIONS: HeaderValue =
    HeaderValue::from_static("camera=(), microphone=(), geolocation=()");
const SAME_ORIGIN: HeaderValue = HeaderValue::from_static("same-origin");
const DENY: HeaderValue = HeaderValue::from_static("DENY");

/// Wraps every request; see the module documentation.
pub(crate) async fn guard<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    req: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let route = route_label(req.extensions().get::<MatchedPath>());
    let flavor = Flavor::for_path(req.uri().path());
    let no_store = method == Method::POST;

    let mut response = admit(&st, req, route, flavor, next).await;

    let headers = response.headers_mut();
    apply_security_headers(headers, st.site.hsts);
    if no_store || !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, NO_STORE);
    }

    let status = response.status();
    metrics::counter!(
        metric_names::HTTP_REQUESTS,
        "route" => route,
        "status" => status.as_str().to_owned()
    )
    .increment(1);
    tracing::info!(
        target: ACCESS_LOG_TARGET,
        method = %method,
        route,
        status = status.as_u16(),
        latency_ms = started.elapsed().as_secs_f64() * 1000.0,
        "request"
    );
    response
}

/// Applies the request limits, then runs the handler.
async fn admit<B: Backend>(
    st: &AppState<B>,
    req: Request,
    route: &'static str,
    flavor: Flavor,
    next: Next,
) -> Response {
    let site = &st.site;
    let uri_len = req.uri().path_and_query().map_or(0, |pq| pq.as_str().len());
    if uri_len > MAX_URI_BYTES {
        return error_response(
            site,
            flavor,
            StatusCode::URI_TOO_LONG,
            "The address is too long.",
        );
    }

    let body_limit = u64::try_from(MAX_BODY_BYTES).unwrap_or(u64::MAX);
    if declared_length(req.headers()).is_some_and(|len| len > body_limit) {
        return error_response(
            site,
            flavor,
            StatusCode::PAYLOAD_TOO_LARGE,
            "The request is too large.",
        );
    }

    let Some(peer) = peer_addr(&req) else {
        if !st.warned_no_peer.swap(true, Ordering::Relaxed) {
            tracing::error!(
                "request without a peer address: serve the router with \
                 into_make_service_with_connect_info::<SocketAddr>()"
            );
        }
        return error_response(
            site,
            flavor,
            StatusCode::INTERNAL_SERVER_ERROR,
            "The server is not configured correctly.",
        );
    };
    let ip = client_ip(peer, req.headers(), &site.trusted_proxies);
    let class = if flavor == Flavor::Json {
        RateClass::Api
    } else {
        RateClass::Page
    };
    if let Verdict::Deny(wait) = st.limiter.check(class, ip, Instant::now()) {
        metrics::counter!(metric_names::RATE_LIMITED, "route" => route).increment(1);
        let mut response = error_response(
            site,
            flavor,
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests. Please wait a moment and try again.",
        );
        response
            .headers_mut()
            .insert(RETRY_AFTER, retry_after(wait));
        return response;
    }

    // Liveness checks are constant-time and must answer under load.
    let _permit = if route == routes::HEALTHZ {
        None
    } else {
        match Arc::clone(&st.requests).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                let mut response = error_response(
                    site,
                    flavor,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "The server is busy. Please try again in a moment.",
                );
                response
                    .headers_mut()
                    .insert(RETRY_AFTER, HeaderValue::from(OVERLOAD_RETRY_SECS));
                return response;
            }
        }
    };

    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(req)).await {
        Ok(response) => response,
        Err(_) => error_response(
            site,
            flavor,
            StatusCode::SERVICE_UNAVAILABLE,
            "The request took too long. Please try again.",
        ),
    }
}

/// Sets the security headers and removes `Server`.
pub(crate) fn apply_security_headers(headers: &mut HeaderMap, hsts: bool) {
    headers.insert(CSP, CSP_VALUE);
    headers.insert(X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    headers.insert(REFERRER_POLICY, NO_REFERRER);
    headers.insert(PERMISSIONS_POLICY, PERMISSIONS);
    headers.insert(CROSS_ORIGIN_OPENER_POLICY, SAME_ORIGIN);
    headers.insert(CROSS_ORIGIN_RESOURCE_POLICY, SAME_ORIGIN);
    headers.insert(X_FRAME_OPTIONS, DENY);
    if hsts {
        headers.insert(STRICT_TRANSPORT_SECURITY, HSTS);
    } else {
        headers.remove(STRICT_TRANSPORT_SECURITY);
    }
    headers.remove(SERVER);
}

/// The route template as a metric label with bounded cardinality.
fn route_label(matched: Option<&MatchedPath>) -> &'static str {
    let Some(matched) = matched.map(MatchedPath::as_str) else {
        return routes::UNMATCHED;
    };
    if matched == static_files::style_path() {
        return routes::STYLE_HASHED;
    }
    routes::ALL
        .iter()
        .copied()
        .find(|r| *r == matched)
        .unwrap_or(routes::UNMATCHED)
}

/// The socket peer recorded by `into_make_service_with_connect_info`, or by
/// `MockConnectInfo` in tests.
fn peer_addr(req: &Request) -> Option<SocketAddr> {
    let ext = req.extensions();
    ext.get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0)
        .or_else(|| ext.get::<MockConnectInfo<SocketAddr>>().map(|m| m.0))
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Whole seconds until a retry can succeed, rounded up, at least 1.
fn retry_after(wait: Duration) -> HeaderValue {
    let secs = wait
        .as_secs()
        .saturating_add(u64::from(wait.subsec_nanos() > 0))
        .max(1);
    HeaderValue::from(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_rounds_up() {
        assert_eq!(retry_after(Duration::ZERO), "1");
        assert_eq!(retry_after(Duration::from_millis(333)), "1");
        assert_eq!(retry_after(Duration::from_secs(20)), "20");
        assert_eq!(retry_after(Duration::from_millis(20_001)), "21");
    }

    #[test]
    fn security_headers_replace_and_remove() {
        let mut h = HeaderMap::new();
        h.insert(SERVER, HeaderValue::from_static("hyper"));
        h.insert(
            STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=1"),
        );
        h.insert(CSP, HeaderValue::from_static("default-src *"));
        apply_security_headers(&mut h, false);
        assert!(h.get(SERVER).is_none());
        assert!(h.get(STRICT_TRANSPORT_SECURITY).is_none());
        assert_eq!(h[CSP], CONTENT_SECURITY_POLICY);
        apply_security_headers(&mut h, true);
        assert_eq!(h[STRICT_TRANSPORT_SECURITY], HSTS_VALUE);
    }

    #[test]
    fn declared_length_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(declared_length(&h), None);
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("32769"));
        assert_eq!(declared_length(&h), Some(32_769));
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("x"));
        assert_eq!(declared_length(&h), None);
    }
}
