//! The home page, information pages, static files and fallbacks.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use dc3_store::MAX_STORED_FILES;

use crate::Backend;
use crate::app::AppState;
use crate::format::{grouped, grouped_signed};
use crate::ratelimit::RATE_LIMIT_IDLE_EXPIRY;
use crate::render::{
    CSS, Flavor, HOME_CACHE, IMMUTABLE_CACHE, STATIC_PAGE_CACHE, TEXT, cached, error_response,
    html, with_type,
};
use crate::static_files::{ROBOTS_TXT, STYLE_CSS};
use crate::templates::{AboutPage, HomePage, PrivacyPage, StatsView};
use crate::theme::{Theme, next_from_uri};

const SECONDS_PER_MINUTE: u64 = 60;

/// `GET /`.
pub(crate) async fn home<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let stats = st.stats().map(|s| StatsView {
        torrents: grouped_signed(s.torrents),
        added_today: grouped_signed(s.added_today),
        added_yesterday: grouped_signed(s.added_yesterday),
    });
    let theme = Theme::from_headers(&headers);
    let next = next_from_uri(&uri);
    let page = HomePage {
        page: st.site.page("Search torrents", "", false, theme, &next),
        stats,
    };
    cached(html(StatusCode::OK, &page), HOME_CACHE)
}

/// `GET /about`.
pub(crate) async fn about<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let theme = Theme::from_headers(&headers);
    let next = next_from_uri(&uri);
    let page = AboutPage {
        page: st.site.page("About", "", true, theme, &next),
    };
    cached(html(StatusCode::OK, &page), STATIC_PAGE_CACHE)
}

/// `GET /privacy`.
pub(crate) async fn privacy<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let theme = Theme::from_headers(&headers);
    let next = next_from_uri(&uri);
    let page = PrivacyPage {
        page: st.site.page("Privacy", "", true, theme, &next),
        stored_files: grouped(u64::try_from(MAX_STORED_FILES).unwrap_or(u64::MAX)),
        rate_limit_minutes: RATE_LIMIT_IDLE_EXPIRY.as_secs() / SECONDS_PER_MINUTE,
    };
    cached(html(StatusCode::OK, &page), STATIC_PAGE_CACHE)
}

/// `GET /robots.txt`.
pub(crate) async fn robots() -> Response {
    cached(
        with_type(StatusCode::OK, TEXT, ROBOTS_TXT),
        STATIC_PAGE_CACHE,
    )
}

/// `GET /.well-known/security.txt` (RFC 9116).
pub(crate) async fn security_txt<B: Backend>(State(st): State<Arc<AppState<B>>>) -> Response {
    cached(
        with_type(StatusCode::OK, TEXT, st.site.security_txt.clone()),
        STATIC_PAGE_CACHE,
    )
}

/// `GET /static/style.css`, for anything that links the plain name.
pub(crate) async fn style() -> Response {
    cached(with_type(StatusCode::OK, CSS, STYLE_CSS), STATIC_PAGE_CACHE)
}

/// `GET /static/style.<hash>.css`: the name changes with the content.
pub(crate) async fn style_immutable() -> Response {
    cached(with_type(StatusCode::OK, CSS, STYLE_CSS), IMMUTABLE_CACHE)
}

/// Any path without a route.
pub(crate) async fn not_found<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let theme = Theme::from_headers(&headers);
    let next = next_from_uri(&uri);
    error_response(
        &st.site,
        Flavor::for_path(uri.path()),
        StatusCode::NOT_FOUND,
        "There is nothing at this address.",
        theme,
        &next,
    )
}

/// A known path with an unsupported method.
pub(crate) async fn method_not_allowed<B: Backend>(
    State(st): State<Arc<AppState<B>>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let theme = Theme::from_headers(&headers);
    let next = next_from_uri(&uri);
    error_response(
        &st.site,
        Flavor::for_path(uri.path()),
        StatusCode::METHOD_NOT_ALLOWED,
        "This address does not accept that kind of request.",
        theme,
        &next,
    )
}
