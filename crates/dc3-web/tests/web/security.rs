//! Output escaping, security headers on every kind of response, and limits.

use axum::body::Body;
use axum::http::{Method, StatusCode};
use dc3_store::TorrentRecord;

use crate::common::*;

const SCRIPT_NAME: &str = "<script>alert(1)</script> hostile";
const BREAKOUT_NAME: &str = "\"><img src=x onerror=alert(1)> hostile";
const BIDI_NAME: &str = "hostile invoice\u{202E}fdp.exe";
const JS_NAME: &str = "javascript:alert(document.cookie) hostile";

fn hostile_torrents() -> Vec<TorrentRecord> {
    vec![
        torrent(
            1,
            SCRIPT_NAME,
            &[
                ("<script>alert(2)</script>/x.txt", 10),
                ("\"><svg onload=alert(3)>.mkv", 20),
            ],
        ),
        torrent(
            2,
            BREAKOUT_NAME,
            &[("dir/\u{202E}gpj.exe", 30), ("' onmouseover='alert(4)", 5)],
        ),
        torrent(3, BIDI_NAME, &[("javascript:alert(5)", 1)]),
        torrent(4, JS_NAME, &[("a&b<c>d\"e'f", 2)]),
    ]
}

/// `assert_safe_html` parses every tag and attribute; these are the
/// specific payloads of the hostile torrents.
fn assert_hostile_text_escaped(body: &str) {
    assert_safe_html(body);
    assert!(!body.contains("<img"));
    assert!(!body.contains("<svg"));
    assert!(!body.contains("\"><img"));
    assert!(!body.contains("\"><svg"));
    assert!(!body.contains("' onmouseover='"));
    assert!(!body.contains("href=\"javascript:"));
}

#[tokio::test]
async fn hostile_names_are_escaped_in_search_results() {
    let _serial = serial().await;
    let app = app(hostile_torrents());
    let r = send(&app.router, get("/search?q=hostile")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_hostile_text_escaped(&r.body);
    assert!(
        r.body
            .contains("<bdi>&#60;script&#62;alert(1)&#60;/script&#62; hostile</bdi>")
    );
    assert!(
        r.body
            .contains("<bdi>&#34;&#62;&#60;img src=x onerror=alert(1)&#62; hostile</bdi>")
    );
    // The bidi override is removed and the name is isolated.
    assert!(r.body.contains("<bdi>hostile invoicefdp.exe</bdi>"));
    assert!(
        r.body
            .contains("<bdi>javascript:alert(document.cookie) hostile</bdi>")
    );
    // Names only reach attributes percent-encoded, inside magnet links.
    assert!(
        r.body
            .contains("dn=javascript%3Aalert%28document.cookie%29%20hostile")
    );
    assert!(
        r.body
            .contains("dn=%3Cscript%3Ealert%281%29%3C%2Fscript%3E%20hostile")
    );
    assert_eq!(r.body.matches("<li class=\"result\">").count(), 4);
}

#[tokio::test]
async fn hostile_names_and_paths_are_escaped_on_torrent_pages() {
    let _serial = serial().await;
    let app = app(hostile_torrents());
    for id in 1..=4 {
        let key = key_for(id).to_hex();
        let r = send(&app.router, get(&format!("/t/{key}"))).await;
        assert_eq!(r.status, StatusCode::OK, "{id}");
        assert_hostile_text_escaped(&r.body);
    }
    let r = send(&app.router, get(&format!("/t/{}", key_for(1).to_hex()))).await;
    assert!(
        r.body
            .contains("<bdi>&#60;script&#62;alert(2)&#60;/script&#62;/x.txt</bdi>")
    );
    assert!(
        r.body
            .contains("<bdi>&#34;&#62;&#60;svg onload=alert(3)&#62;.mkv</bdi>")
    );
    // The page title is escaped as well.
    assert!(
        r.body
            .contains("<title>&#60;script&#62;alert(1)&#60;/script&#62; hostile · ")
    );
    let r = send(&app.router, get(&format!("/t/{}", key_for(2).to_hex()))).await;
    assert!(r.body.contains("<bdi>dir/gpj.exe</bdi>"));
    assert!(
        r.body
            .contains("<bdi>&#39; onmouseover=&#39;alert(4)</bdi>")
    );
    let r = send(&app.router, get(&format!("/t/{}", key_for(4).to_hex()))).await;
    assert!(
        r.body
            .contains("<bdi>a&#38;b&#60;c&#62;d&#34;e&#39;f</bdi>")
    );
}

#[tokio::test]
async fn hostile_names_are_escaped_in_json() {
    let _serial = serial().await;
    let app = app(hostile_torrents());
    let r = send(&app.router, get("/api/v1/search?q=hostile")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains('<'));
    assert!(!r.body.contains('>'));
    assert!(!r.body.contains('\u{202E}'));
    assert!(r.body.contains("\\u003cscript\\u003e"));
    let json = r.json();
    let names: Vec<&str> = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&SCRIPT_NAME));
    assert!(names.contains(&BREAKOUT_NAME));
    assert!(names.contains(&"hostile invoicefdp.exe"));
    assert!(names.contains(&JS_NAME));

    let r = send(
        &app.router,
        get(&format!("/api/v1/torrents/{}", key_for(1).to_hex())),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("<script"));
    assert!(!r.body.contains('<'));
    let json = r.json();
    assert_eq!(json["files"][0]["path"], "<script>alert(2)</script>/x.txt");
    assert_eq!(json["files"][1]["path"], "\"><svg onload=alert(3)>.mkv");
}

#[tokio::test]
async fn searching_for_markup_is_escaped() {
    let _serial = serial().await;
    let app = app(hostile_torrents());
    let q = "hostile \"><script>x</script>";
    let r = send(&app.router, get(&format!("/search?q={}", enc(q)))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_safe_html(&r.body);
    assert!(
        r.body
            .contains("value=\"hostile &#34;&#62;&#60;script&#62;x&#60;/script&#62;\"")
    );
}

#[tokio::test]
async fn every_kind_of_response_has_the_security_headers() {
    let _serial = serial().await;
    let app = app(hostile_torrents());
    let key = key_for(1).to_hex();
    let missing = key_for(77).to_hex();
    let cases: Vec<(Method, String, StatusCode)> = vec![
        (Method::GET, "/".into(), StatusCode::OK),
        (Method::GET, "/search?q=hostile".into(), StatusCode::OK),
        (Method::GET, "/search".into(), StatusCode::SEE_OTHER),
        (
            Method::GET,
            "/search?q=%20%20".into(),
            StatusCode::SEE_OTHER,
        ),
        (
            Method::GET,
            "/search?q=x&p=0".into(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/search?q=x&sort=up".into(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/search?q=-only".into(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            "/search?q=zzforbiddenzz".into(),
            StatusCode::OK,
        ),
        (Method::GET, format!("/t/{key}"), StatusCode::OK),
        (Method::GET, "/t/not-a-key".into(), StatusCode::BAD_REQUEST),
        (Method::GET, format!("/t/{missing}"), StatusCode::NOT_FOUND),
        (Method::GET, format!("/report/{key}"), StatusCode::OK),
        (Method::GET, "/report/zz".into(), StatusCode::BAD_REQUEST),
        (
            Method::GET,
            "/api/v1/search?q=hostile".into(),
            StatusCode::OK,
        ),
        (
            Method::GET,
            "/api/v1/search".into(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            format!("/api/v1/torrents/{key}"),
            StatusCode::OK,
        ),
        (
            Method::GET,
            "/api/v1/torrents/x".into(),
            StatusCode::BAD_REQUEST,
        ),
        (
            Method::GET,
            format!("/api/v1/torrents/{missing}"),
            StatusCode::NOT_FOUND,
        ),
        (Method::GET, "/about".into(), StatusCode::OK),
        (Method::GET, "/legal".into(), StatusCode::OK),
        (Method::GET, "/privacy".into(), StatusCode::OK),
        (Method::GET, "/robots.txt".into(), StatusCode::OK),
        (
            Method::GET,
            "/.well-known/security.txt".into(),
            StatusCode::OK,
        ),
        (Method::GET, "/static/style.css".into(), StatusCode::OK),
        (Method::GET, "/healthz".into(), StatusCode::OK),
        (Method::GET, "/readyz".into(), StatusCode::OK),
        (Method::GET, "/does-not-exist".into(), StatusCode::NOT_FOUND),
        (
            Method::PATCH,
            "/legal".into(),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            Method::GET,
            format!("/search?q={}", "x".repeat(5000)),
            StatusCode::URI_TOO_LONG,
        ),
    ];
    for (i, (method, uri, expected)) in cases.into_iter().enumerate() {
        // A fresh client per request keeps the rate limiter out of the way.
        let peer = format!("192.0.2.{}:4000", i + 1);
        let r = send(&app.router, request(method, &uri, Body::empty(), &peer)).await;
        assert_eq!(r.status, expected, "{uri}");
        assert_security_headers(&r, true);
        let html = r
            .headers
            .get("content-type")
            .is_some_and(|v| v.to_str().unwrap().starts_with("text/html"));
        if html {
            assert_safe_html(&r.body);
        }
    }

    // A refused report (CSRF) and an oversized one.
    let r = send(
        &app.router,
        report_post(
            &key,
            &[("sec-fetch-site", "cross-site")],
            "reason=other",
            CLIENT,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_security_headers(&r, true);
    assert_eq!(r.header("cache-control"), "no-store");
    let big = format!("reason=other&message={}", "a".repeat(40_000));
    let r = send(
        &app.router,
        report_post(&key, &SAME_ORIGIN_HEADERS, &big, CLIENT),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_security_headers(&r, true);

    // The redirect goes to the home page only.
    let r = send(&app.router, get_from("/search?q=", "192.0.2.200:1")).await;
    assert_eq!(r.header("location"), "/");
}

#[tokio::test]
async fn rate_limited_responses_have_headers_and_retry_after() {
    let _serial = serial().await;
    let app = app(Vec::new());
    let before = metrics().counter("dc3_rate_limited_total{route=/about}");
    let peer = "203.0.113.50:1000";
    let mut limited = None;
    // Pages allow a burst of 30.
    for _ in 0..40 {
        let r = send(&app.router, get_from("/about", peer)).await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            limited = Some(r);
            break;
        }
        assert_eq!(r.status, StatusCode::OK);
    }
    let r = limited.expect("the page burst was never exhausted");
    assert_security_headers(&r, true);
    assert_eq!(r.header("retry-after"), "1");
    assert_eq!(r.header("cache-control"), "no-store");
    assert!(r.body.contains("Too many requests"));
    assert_safe_html(&r.body);
    assert_eq!(
        metrics().counter("dc3_rate_limited_total{route=/about}"),
        before + 1
    );
    // Other clients are not affected.
    let other = send(&app.router, get_from("/about", "203.0.113.51:1000")).await;
    assert_eq!(other.status, StatusCode::OK);

    // The API has its own budget and answers in JSON.
    let mut api_limited = None;
    for _ in 0..30 {
        let r = send(&app.router, get_from("/api/v1/search?q=a", peer)).await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            api_limited = Some(r);
            break;
        }
    }
    let r = api_limited.expect("the API burst was never exhausted");
    assert!(!r.json()["error"].as_str().unwrap().is_empty());
    assert_security_headers(&r, true);
}

#[tokio::test]
async fn forwarded_addresses_are_used_only_from_trusted_proxies() {
    let _serial = serial().await;
    let mut cfg = config();
    cfg.trusted_proxies = vec!["10.0.0.0/8".parse().unwrap()];
    let app = app_with(Vec::new(), cfg);
    let proxy = "10.1.2.3:5555";
    let forwarded = |client: &str| {
        let mut req = get_from("/legal", proxy);
        req.headers_mut().insert(
            "x-forwarded-for",
            format!("6.6.6.6, {client}").parse().unwrap(),
        );
        req
    };
    // Exhaust one forwarded client's burst through the proxy.
    let mut statuses = Vec::new();
    for _ in 0..31 {
        statuses.push(send(&app.router, forwarded("198.51.100.77")).await.status);
    }
    assert_eq!(statuses.last(), Some(&StatusCode::TOO_MANY_REQUESTS));
    // Another client behind the same proxy is still served, although the
    // spoofable leftmost entry is identical.
    let r = send(&app.router, forwarded("198.51.100.78")).await;
    assert_eq!(r.status, StatusCode::OK);

    // An untrusted peer's header is ignored: its own address is limited.
    let direct = "203.0.113.9:1";
    for _ in 0..30 {
        let mut req = get_from("/legal", direct);
        req.headers_mut()
            .insert("x-forwarded-for", "198.51.100.200".parse().unwrap());
        assert_eq!(send(&app.router, req).await.status, StatusCode::OK);
    }
    let mut req = get_from("/legal", direct);
    req.headers_mut()
        .insert("x-forwarded-for", "198.51.100.201".parse().unwrap());
    assert_eq!(
        send(&app.router, req).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn hsts_is_only_sent_when_configured() {
    let _serial = serial().await;
    let mut cfg = config();
    cfg.hsts = false;
    let app = app_with(Vec::new(), cfg);
    let r = send(&app.router, get("/")).await;
    assert_security_headers(&r, false);
    let r = send(&app.router, get("/nope")).await;
    assert_security_headers(&r, false);
}

#[tokio::test]
async fn request_bodies_are_limited_outside_reports_too() {
    let _serial = serial().await;
    let app = app(Vec::new());
    let mut req = request(
        Method::GET,
        "/about",
        Body::from(vec![b'a'; 4 * 1024 + 1]),
        CLIENT,
    );
    req.headers_mut()
        .insert("content-length", "4097".parse().unwrap());
    let r = send(&app.router, req).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_security_headers(&r, true);
    let mut req = request(Method::GET, "/about", Body::from(vec![b'a'; 4096]), CLIENT);
    req.headers_mut()
        .insert("content-length", "4096".parse().unwrap());
    assert_eq!(send(&app.router, req).await.status, StatusCode::OK);
}
