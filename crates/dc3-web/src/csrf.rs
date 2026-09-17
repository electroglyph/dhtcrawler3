//! Cross-site request forgery check for form posts (`docs/03-design.md` §12).

use axum::http::header::ORIGIN;
use axum::http::{HeaderMap, HeaderName};

/// The `Sec-Fetch-Site` request header (Fetch Metadata).
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

/// True if a POST with these headers may be processed.
///
/// A browser's `Sec-Fetch-Site` must be `same-origin` or `none`. Without that
/// header (older browsers), `Origin` must equal the site's public origin.
/// Anything else, including a request with neither header, is refused.
pub(crate) fn post_allowed(headers: &HeaderMap, origin: &str) -> bool {
    if let Some(site) = headers.get(SEC_FETCH_SITE) {
        return site.to_str().is_ok_and(|site| {
            site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none")
        });
    }
    match headers.get(ORIGIN).map(|v| v.to_str()) {
        Some(Ok(value)) => !origin.is_empty() && value.eq_ignore_ascii_case(origin),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const SITE: &str = "https://search.example.org";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        h
    }

    #[test]
    fn same_origin_and_user_initiated_are_accepted() {
        assert!(post_allowed(
            &headers(&[("sec-fetch-site", "same-origin")]),
            SITE
        ));
        assert!(post_allowed(&headers(&[("sec-fetch-site", "none")]), SITE));
        assert!(post_allowed(
            &headers(&[("sec-fetch-site", "Same-Origin")]),
            SITE
        ));
    }

    #[test]
    fn cross_site_and_same_site_are_refused_even_with_a_matching_origin() {
        for site in ["cross-site", "same-site", "", "bogus"] {
            let h = headers(&[("sec-fetch-site", site), ("origin", SITE)]);
            assert!(!post_allowed(&h, SITE), "{site}");
        }
    }

    #[test]
    fn origin_is_checked_when_fetch_metadata_is_absent() {
        assert!(post_allowed(&headers(&[("origin", SITE)]), SITE));
        assert!(post_allowed(
            &headers(&[("origin", "HTTPS://SEARCH.EXAMPLE.ORG")]),
            SITE
        ));
        for origin in [
            "https://evil.example",
            "null",
            "https://search.example.org.evil.example",
            "http://search.example.org",
            "https://search.example.org:8443",
            "",
        ] {
            assert!(
                !post_allowed(&headers(&[("origin", origin)]), SITE),
                "{origin}"
            );
        }
    }

    #[test]
    fn neither_header_is_refused() {
        assert!(!post_allowed(&HeaderMap::new(), SITE));
        assert!(!post_allowed(
            &headers(&[("referer", "https://search.example.org/")]),
            SITE
        ));
    }

    #[test]
    fn empty_configured_origin_never_matches() {
        assert!(!post_allowed(&headers(&[("origin", "")]), ""));
    }

    #[test]
    fn non_utf8_fetch_metadata_is_refused() {
        let mut h = HeaderMap::new();
        h.insert(
            SEC_FETCH_SITE,
            HeaderValue::from_bytes(b"same\xff").unwrap(),
        );
        h.insert(ORIGIN, HeaderValue::from_static(SITE));
        assert!(!post_allowed(&h, SITE));
    }
}
