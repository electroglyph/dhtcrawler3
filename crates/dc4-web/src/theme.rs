//! The dark/light theme preference (`docs/03-design.md` §12).
//!
//! Pages are dark unless the visitor chose light with the header switch.
//! The switch is a plain form posting to `POST /theme`; the server stores
//! the choice in a `theme` cookie and renders every page in that theme, so
//! no JavaScript is needed and the choice survives navigation.
//!
//! The cookie holds only `dark` or `light`. It is set only when the switch
//! is used; visitors who never touch it get no cookie at all.

use axum::http::{HeaderMap, HeaderValue, Uri};

/// The `Cookie` request header.
const COOKIE: &str = "cookie";
/// Name of the theme preference cookie.
pub(crate) const COOKIE_NAME: &str = "theme";
/// How long the preference lasts, in seconds (one year).
pub(crate) const MAX_AGE_SECS: u64 = 365 * 24 * 60 * 60;
/// Longest `next` redirect target kept (see [`sanitize_next`]).
const MAX_NEXT_BYTES: usize = 1024;

/// Dark or light page theme. Dark is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    /// The theme named by the request's `theme` cookie, or dark when the
    /// cookie is missing or unusable.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Theme {
        for values in headers.get_all(COOKIE).into_iter() {
            if let Ok(header) = values.to_str() {
                for cookie in header.split(';') {
                    let mut parts = cookie.splitn(2, '=');
                    let name = parts.next().unwrap_or("").trim();
                    let value = parts.next().unwrap_or("").trim();
                    if name == COOKIE_NAME
                        && let Some(theme) = Theme::from_value(value)
                    {
                        return theme;
                    }
                }
            }
        }
        Theme::Dark
    }

    /// Parses a submitted theme value. Only the exact values the switch
    /// sends are accepted.
    pub(crate) fn from_value(value: &str) -> Option<Theme> {
        match value {
            "dark" => Some(Theme::Dark),
            "light" => Some(Theme::Light),
            _ => None,
        }
    }

    /// The theme as rendered into pages and the cookie.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    /// The theme the switch offers from here.
    pub(crate) fn opposite(self) -> Theme {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }

    /// The switch button text from here ("Light mode" on a dark page).
    pub(crate) fn label(self) -> &'static str {
        match self {
            Theme::Dark => "Light mode",
            Theme::Light => "Dark mode",
        }
    }

    /// The `Set-Cookie` value storing this theme: a bare preference for
    /// this site only. `SameSite=Lax` keeps other sites from changing it
    /// through top-level navigation tricks; there is nothing secret in it
    /// (no `Secure` flag, so plain-`http` sites keep working).
    pub(crate) fn set_cookie(self) -> HeaderValue {
        let mut value = format!(
            "{COOKIE_NAME}={}; Path=/; Max-Age={MAX_AGE_SECS}; SameSite=Lax",
            self.as_str()
        );
        // `HeaderValue::from_str` cannot fail: every byte is visible ASCII.
        HeaderValue::from_str(&value).unwrap_or_else(|_| {
            value.clear();
            HeaderValue::from_str("theme=dark; Path=/").unwrap_or(HeaderValue::from_static(""))
        })
    }

    /// Keeps a `next` redirect target if it is a local path, else `/`.
    /// Only same-origin paths are kept (starting with one `/`, with no
    /// backslash, whitespace or control characters), so a submitted form
    /// cannot bounce a visitor to another site. Targets longer than
    /// [`MAX_NEXT_BYTES`] fall back to `/` so the switch form stays well
    /// under the request body limit even on long addresses.
    pub(crate) fn sanitize_next(next: &str) -> String {
        let valid = next.len() <= MAX_NEXT_BYTES
            && next.starts_with('/')
            && !next.starts_with("//")
            && !next.chars().any(|c| c.is_control() || c.is_whitespace())
            && !next.contains('\\');
        if valid {
            next.to_owned()
        } else {
            "/".to_owned()
        }
    }
}

/// The current request address as the theme switch's return target: the
/// path with its query string, sanitized to a local path by
/// [`Theme::sanitize_next`].
pub(crate) fn next_from_uri(uri: &Uri) -> String {
    let current = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(uri.path());
    Theme::sanitize_next(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(COOKIE, HeaderValue::from_str(cookie).unwrap());
        h
    }

    #[test]
    fn cookie_parsing() {
        assert_eq!(Theme::from_headers(&HeaderMap::new()), Theme::Dark);
        assert_eq!(Theme::from_headers(&headers("theme=light")), Theme::Light);
        assert_eq!(Theme::from_headers(&headers("theme=dark")), Theme::Dark);
        assert_eq!(
            Theme::from_headers(&headers("other=1; theme=light; x=2")),
            Theme::Light
        );
        assert_eq!(
            Theme::from_headers(&headers(" theme = light ")),
            Theme::Light
        );
        // Unknown values fall back to dark.
        assert_eq!(Theme::from_headers(&headers("theme=pink")), Theme::Dark);
        assert_eq!(Theme::from_headers(&headers("theme=")), Theme::Dark);
        assert_eq!(Theme::from_headers(&headers("theme=Light")), Theme::Dark);
        assert_eq!(Theme::from_headers(&headers("other=1")), Theme::Dark);
    }

    #[test]
    fn values_and_labels() {
        assert_eq!(Theme::from_value("dark"), Some(Theme::Dark));
        assert_eq!(Theme::from_value("light"), Some(Theme::Light));
        assert_eq!(Theme::from_value(""), None);
        assert_eq!(Theme::from_value("DARK"), None);
        assert_eq!(Theme::Dark.opposite(), Theme::Light);
        assert_eq!(Theme::Light.opposite(), Theme::Dark);
        assert_eq!(Theme::Dark.label(), "Light mode");
        assert_eq!(Theme::Light.label(), "Dark mode");
    }

    #[test]
    fn set_cookie_shape() {
        let light = Theme::Light.set_cookie().to_str().unwrap().to_owned();
        assert!(light.starts_with("theme=light; "), "{light}");
        assert!(light.contains("Path=/"), "{light}");
        assert!(light.contains("SameSite=Lax"), "{light}");
        assert!(light.contains("Max-Age=31536000"), "{light}");
        let dark = Theme::Dark.set_cookie().to_str().unwrap().to_owned();
        assert!(dark.starts_with("theme=dark; "), "{dark}");
    }

    #[test]
    fn next_targets_stay_local() {
        assert_eq!(Theme::sanitize_next("/"), "/");
        assert_eq!(Theme::sanitize_next("/t/abc?q=x&p=2"), "/t/abc?q=x&p=2");
        for bad in [
            "",
            "https://evil.example/",
            "//evil.example/",
            "/\\evil.example",
            "/has space",
            "/new\nline",
            "relative/path",
            "?q=x",
        ] {
            assert_eq!(Theme::sanitize_next(bad), "/", "{bad:?}");
        }
        assert_eq!(Theme::sanitize_next(&"a".repeat(4096)), "/");
        let long_local = format!("/search?q={}", "x".repeat(2000));
        assert_eq!(Theme::sanitize_next(&long_local), "/");
    }
}
