//! Building responses: pages, JSON, plain text and errors.

use askama::Template;
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use serde::Serialize;

use crate::app::{Site, routes};
use crate::templates::ErrorPage;

pub(crate) const HTML: HeaderValue = HeaderValue::from_static("text/html; charset=utf-8");
pub(crate) const JSON: HeaderValue = HeaderValue::from_static("application/json");
pub(crate) const TEXT: HeaderValue = HeaderValue::from_static("text/plain; charset=utf-8");
pub(crate) const CSS: HeaderValue = HeaderValue::from_static("text/css; charset=utf-8");

/// Dynamic responses, report pages and form posts.
pub(crate) const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");
/// Information pages, robots.txt, security.txt and the unhashed stylesheet.
pub(crate) const STATIC_PAGE_CACHE: HeaderValue = HeaderValue::from_static("public, max-age=300");
/// The home page, whose statistics change at most once a minute.
pub(crate) const HOME_CACHE: HeaderValue = HeaderValue::from_static("public, max-age=60");
/// The stylesheet whose name carries its hash.
pub(crate) const IMMUTABLE_CACHE: HeaderValue =
    HeaderValue::from_static("public, max-age=31536000, immutable");

/// A fallback body if serialising an error message ever failed.
const JSON_INTERNAL_ERROR: &[u8] = br#"{"error":"internal error"}"#;

/// Whether error responses for a path are HTML pages or JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Html,
    Json,
}

impl Flavor {
    /// JSON for the API, HTML everywhere else.
    pub(crate) fn for_path(path: &str) -> Flavor {
        let api_root = routes::API_PREFIX.trim_end_matches('/');
        if path == api_root || path.starts_with(routes::API_PREFIX) {
            Flavor::Json
        } else {
            Flavor::Html
        }
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

/// A rendered page. A rendering failure (which only a broken writer could
/// cause) becomes a plain 500.
pub(crate) fn html<T: Template>(status: StatusCode, page: &T) -> Response {
    match page.render() {
        Ok(body) => with_type(status, HTML, body),
        Err(e) => {
            tracing::error!(error = %e, "page rendering failed");
            with_type(
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                "Internal server error\n",
            )
        }
    }
}

/// A JSON response built from `value`.
pub(crate) fn json<T: Serialize + ?Sized>(status: StatusCode, value: &T) -> Response {
    match crate::json::to_vec(value) {
        Ok(body) => with_type(status, JSON, body),
        Err(e) => {
            tracing::error!(error = %e, "JSON serialisation failed");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// `{"error": message}`.
pub(crate) fn json_error(status: StatusCode, message: &str) -> Response {
    let body = crate::json::to_vec(&ErrorBody { error: message })
        .unwrap_or_else(|_| JSON_INTERNAL_ERROR.to_vec());
    with_type(status, JSON, body)
}

/// A plain-text response.
pub(crate) fn text(status: StatusCode, body: impl Into<Body>) -> Response {
    with_type(status, TEXT, body)
}

/// A response with the given status, content type and body.
pub(crate) fn with_type(
    status: StatusCode,
    content_type: HeaderValue,
    body: impl Into<Body>,
) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    response
}

/// Sets `Cache-Control`.
pub(crate) fn cached(mut response: Response, value: HeaderValue) -> Response {
    response.headers_mut().insert(CACHE_CONTROL, value);
    response
}

/// An error as an HTML page or as JSON.
pub(crate) fn error_response(
    site: &Site,
    flavor: Flavor,
    status: StatusCode,
    message: &str,
) -> Response {
    error_with_query(site, flavor, status, message, "")
}

/// Like [`error_response`], with the header search box prefilled.
pub(crate) fn error_with_query(
    site: &Site,
    flavor: Flavor,
    status: StatusCode,
    message: &str,
    query: &str,
) -> Response {
    match flavor {
        Flavor::Json => json_error(status, message),
        Flavor::Html => {
            let heading = heading(status);
            let page = ErrorPage {
                page: site.page(heading, query, true),
                heading,
                message,
            };
            html(status, &page)
        }
    }
}

fn heading(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "Bad request",
        StatusCode::FORBIDDEN => "Not allowed",
        StatusCode::NOT_FOUND => "Not found",
        StatusCode::METHOD_NOT_ALLOWED => "Method not allowed",
        StatusCode::PAYLOAD_TOO_LARGE => "Request too large",
        StatusCode::URI_TOO_LONG => "Address too long",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "Unsupported request",
        StatusCode::TOO_MANY_REQUESTS => "Too many requests",
        StatusCode::SERVICE_UNAVAILABLE => "Temporarily unavailable",
        StatusCode::INTERNAL_SERVER_ERROR => "Something went wrong",
        other => other.canonical_reason().unwrap_or("Error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_by_path() {
        assert_eq!(Flavor::for_path("/api/v1/search"), Flavor::Json);
        assert_eq!(Flavor::for_path("/api/nope"), Flavor::Json);
        assert_eq!(Flavor::for_path("/api"), Flavor::Json);
        assert_eq!(Flavor::for_path("/apiary"), Flavor::Html);
        assert_eq!(Flavor::for_path("/"), Flavor::Html);
    }

    #[test]
    fn json_error_shape() {
        let r = json_error(StatusCode::NOT_FOUND, "no <such> thing");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(r.headers()[CONTENT_TYPE], "application/json");
    }
}
