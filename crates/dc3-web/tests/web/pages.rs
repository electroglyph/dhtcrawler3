//! Home, information pages, static files, health checks and fallbacks.

use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Method, StatusCode};

use crate::common::*;

#[tokio::test]
async fn home_page_has_a_labelled_search_box_and_no_script() {
    let _serial = serial().await;
    let app = app(Vec::new());
    let r = send(&app.router, get("/")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), "text/html; charset=utf-8");
    assert_eq!(r.header("cache-control"), "public, max-age=60");
    assert_security_headers(&r, true);
    assert_safe_html(&r.body);
    assert!(r.body.contains(r#"<label for="q">Search torrents</label>"#));
    assert!(
        r.body
            .contains(r#"<input id="q" name="q" type="search" maxlength="200""#)
    );
    assert!(
        r.body
            .contains(r#"<form class="search-form" action="/search" method="get""#)
    );
    // The configured site name is escaped.
    assert!(r.body.contains("Test &#60;Search&#62; &#38; Co"));
    assert!(!r.body.contains(SITE_NAME));
    // Without serve() no statistics have been loaded.
    assert!(r.body.contains("Index statistics are not available yet."));
    // Footer: only the source link.
    assert!(
        r.body.contains(
            r#"Source code available at <a href="https://github.com/electroglyph/dhtcrawler4">https://github.com/electroglyph/dhtcrawler4</a>"#
        )
    );
    assert!(!r.body.contains(r#"<a href="/about">About</a>"#));
    assert!(!r.body.contains(r#"<a href="/privacy">Privacy</a>"#));
    // Dark by default; the header form stores the light choice in a
    // cookie (POST /theme) without JavaScript.
    assert!(r.body.contains(r#"<html lang="en" data-theme="dark">"#));
    assert!(
        r.body
            .contains(r#"<meta name="color-scheme" content="dark">"#)
    );
    assert!(
        r.body
            .contains(r#"<form class="theme-switch" action="/theme" method="post">"#)
    );
    assert!(
        r.body
            .contains(r#"<input type="hidden" name="theme" value="light">"#)
    );
    assert!(
        r.body
            .contains(r#"<input type="hidden" name="next" value="/">"#)
    );
    assert!(
        r.body
            .contains(r#"<button type="submit">Light mode</button>"#)
    );
    // Themed pages vary by cookie and set no cookie unasked.
    assert_eq!(r.header("vary"), "Cookie");
    assert!(r.headers.get("set-cookie").is_none());
    // The stylesheet link carries a content hash and is served immutable.
    let start = r.body.find("/static/style.").unwrap();
    let end = start + r.body[start..].find('"').unwrap();
    let css_path = &r.body[start..end];
    assert_eq!(css_path.len(), "/static/style.".len() + 16 + ".css".len());
    let css = send(&app.router, get(css_path)).await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.header("content-type"), "text/css; charset=utf-8");
    assert_eq!(
        css.header("cache-control"),
        "public, max-age=31536000, immutable"
    );
    assert!(css.body.contains("--bg: #0f1216"));
    assert!(css.body.contains(":root[data-theme=\"light\"]"));
    assert!(css.body.contains("--bg: #ffffff"));
    assert!(!css.body.contains("prefers-color-scheme"));
    assert!(!css.body.contains(":has(#theme-toggle"));
    assert_security_headers(&css, true);
    let plain = send(&app.router, get("/static/style.css")).await;
    assert_eq!(plain.status, StatusCode::OK);
    assert_eq!(plain.body, css.body);
    assert_eq!(plain.header("cache-control"), "public, max-age=300");
}

#[tokio::test]
async fn information_pages() {
    let _serial = serial().await;
    let app = app(Vec::new());
    for (path, needle) in [
        ("/about", "How it works"),
        ("/privacy", "What is never stored"),
    ] {
        let r = send(&app.router, get(path)).await;
        assert_eq!(r.status, StatusCode::OK, "{path}");
        assert!(r.body.contains(needle), "{path}");
        assert_eq!(r.header("cache-control"), "public, max-age=300");
        assert_eq!(r.header("vary"), "Cookie");
        assert_security_headers(&r, true);
        assert_safe_html(&r.body);
    }

    let privacy = send(&app.router, get("/privacy")).await;
    for needle in [
        "first 2,000 files",
        "Your IP address.",
        "Your searches.",
        "IP addresses of BitTorrent peers",
        "forgotten after 10 minutes",
        "at most 45 minutes",
        "only cookie",
        "theme",
    ] {
        assert!(privacy.body.contains(needle), "{needle}");
    }
}

#[tokio::test]
async fn robots_and_security_txt() {
    let _serial = serial().await;
    let app = app(Vec::new());
    let robots = send(&app.router, get("/robots.txt")).await;
    assert_eq!(robots.status, StatusCode::OK);
    assert_eq!(robots.header("content-type"), "text/plain; charset=utf-8");
    assert_eq!(
        robots.body,
        "User-agent: *\nDisallow: /search\nDisallow: /api/\nDisallow: /t/\n"
    );
    assert_security_headers(&robots, true);

    let txt = send(&app.router, get("/.well-known/security.txt")).await;
    assert_eq!(txt.status, StatusCode::OK);
    assert_eq!(txt.header("content-type"), "text/plain; charset=utf-8");
    let lines: Vec<&str> = txt.body.lines().collect();
    assert_eq!(lines[0], "Contact: https://search.example.org/");
    let expires = lines[1].strip_prefix("Expires: ").unwrap();
    let expires = chrono::DateTime::parse_from_rfc3339(expires).unwrap();
    let days = (expires.to_utc() - chrono::Utc::now()).num_days();
    assert!((363..=365).contains(&days), "{days}");
    assert!(lines[1].ends_with('Z'));
    assert_eq!(
        lines[2],
        "Canonical: https://search.example.org/.well-known/security.txt"
    );
    assert_security_headers(&txt, true);
}

#[tokio::test]
async fn health_checks() {
    let _serial = serial().await;
    let app = app(Vec::new());
    let r = send(&app.router, get("/healthz")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, "ok");
    assert_eq!(r.header("cache-control"), "no-store");
    assert_security_headers(&r, true);

    let ready = send(&app.router, get("/readyz")).await;
    assert_eq!(ready.status, StatusCode::OK);
    assert_eq!(ready.body, "ready");

    app.backend.0.ping_fails.store(true, Ordering::SeqCst);
    let not_ready = send(&app.router, get("/readyz")).await;
    assert_eq!(not_ready.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(not_ready.body, "not ready");
    assert_security_headers(&not_ready, true);
    // Liveness does not depend on the database.
    assert_eq!(
        send(&app.router, get("/healthz")).await.status,
        StatusCode::OK
    );

    app.backend.0.ping_fails.store(false, Ordering::SeqCst);
    assert_eq!(
        send(&app.router, get("/readyz")).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn unknown_routes_get_a_404_page_with_headers() {
    let _serial = serial().await;
    let app = app(Vec::new());
    for path in [
        "/nope",
        "/t",
        "/t/",
        "/about/",
        "/static/other.css",
        "/.env",
    ] {
        let r = send(&app.router, get(path)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(r.header("content-type"), "text/html; charset=utf-8");
        assert!(r.body.contains("<h1 id=\"error-heading\">Not found</h1>"));
        assert!(
            r.body.contains(
                r#"Source code available at <a href="https://github.com/electroglyph/dhtcrawler4">https://github.com/electroglyph/dhtcrawler4</a>"#
            )
        );
        assert_security_headers(&r, true);
        assert_safe_html(&r.body);
    }
    // Unknown API paths answer in JSON.
    let api = send(&app.router, get("/api/v2/whatever")).await;
    assert_eq!(api.status, StatusCode::NOT_FOUND);
    assert_eq!(api.json()["error"], "There is nothing at this address.");
    assert_security_headers(&api, true);
    let metric = "dc3_http_requests_total{route=unmatched,status=404}";
    assert!(metrics().counter(metric) >= 7);
}

#[tokio::test]
async fn wrong_method_gets_405_with_headers() {
    let _serial = serial().await;
    let app = app(Vec::new());
    for (method, path) in [
        (Method::DELETE, "/about"),
        (Method::POST, "/search"),
        (Method::PUT, "/t/0000000000000000000000000000000000000000"),
        (Method::POST, "/api/v1/search"),
    ] {
        let r = send(&app.router, request(method, path, Body::empty(), CLIENT)).await;
        assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert!(r.headers.get("allow").is_some());
        assert_security_headers(&r, true);
    }
    // POST responses are never cached.
    let r = send(
        &app.router,
        request(Method::POST, "/search", Body::empty(), CLIENT),
    )
    .await;
    assert_eq!(r.header("cache-control"), "no-store");
}

#[tokio::test]
async fn head_requests_work() {
    let _serial = serial().await;
    let app = app(Vec::new());
    let r = send(
        &app.router,
        request(Method::HEAD, "/about", Body::empty(), CLIENT),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.is_empty());
    assert_security_headers(&r, true);
}

#[tokio::test]
async fn requests_without_a_peer_address_are_refused() {
    let _serial = serial().await;
    let app = app(Vec::new());
    let req = axum::http::Request::builder()
        .uri("/about")
        .body(Body::empty())
        .unwrap();
    let r = send(&app.router, req).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_security_headers(&r, true);
}
