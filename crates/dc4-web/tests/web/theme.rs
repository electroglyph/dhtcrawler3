//! The dark/light theme switch: `POST /theme` stores a cookie, pages
//! render in the stored theme, all without JavaScript.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};

use crate::common::*;

fn post_form(uri: &str, body: &str) -> Request<Body> {
    let mut req = request(Method::POST, uri, Body::from(body.to_owned()), CLIENT);
    req.headers_mut().insert(
        "content-type",
        "application/x-www-form-urlencoded".parse().unwrap(),
    );
    req
}

fn get_with_cookie(uri: &str, cookie: &str) -> Request<Body> {
    let mut req = get(uri);
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    req
}

fn assert_dark(r: &TestResponse) {
    assert!(r.body.contains(r#"<html lang="en" data-theme="dark">"#));
    assert!(
        r.body
            .contains(r#"<meta name="color-scheme" content="dark">"#)
    );
    assert!(
        r.body
            .contains(r#"<button type="submit">Light mode</button>"#)
    );
    assert!(
        r.body
            .contains(r#"<input type="hidden" name="theme" value="light">"#)
    );
}

fn assert_light(r: &TestResponse) {
    assert!(r.body.contains(r#"<html lang="en" data-theme="light">"#));
    assert!(
        r.body
            .contains(r#"<meta name="color-scheme" content="light">"#)
    );
    assert!(
        r.body
            .contains(r#"<button type="submit">Dark mode</button>"#)
    );
    assert!(
        r.body
            .contains(r#"<input type="hidden" name="theme" value="dark">"#)
    );
}

#[tokio::test]
async fn theme_switch_stores_a_cookie_and_redirects_back() {
    let _serial = serial().await;
    let app = app(Vec::new());

    let r = send(&app.router, post_form("/theme", "theme=light&next=/about")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.header("location"), "/about");
    let cookie = r.header("set-cookie").to_owned();
    assert!(cookie.starts_with("theme=light;"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Max-Age=31536000"), "{cookie}");
    assert_eq!(r.header("cache-control"), "no-store");
    assert_security_headers(&r, true);

    // The stored theme renders on the next page, with a way back.
    let r = send(&app.router, get_with_cookie("/about", &cookie)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_light(&r);
    assert!(
        r.body
            .contains(r#"<input type="hidden" name="next" value="/about">"#)
    );
    assert_eq!(r.header("vary"), "Cookie");
    assert_security_headers(&r, true);
    assert_safe_html(&r.body);

    // And back to dark.
    let r = send(&app.router, post_form("/theme", "theme=dark&next=/")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.header("location"), "/");
    let cookie = r.header("set-cookie").to_owned();
    assert!(cookie.starts_with("theme=dark;"), "{cookie}");
    let r = send(&app.router, get_with_cookie("/", &cookie)).await;
    assert_dark(&r);
}

#[tokio::test]
async fn theme_redirects_stay_on_this_site() {
    let _serial = serial().await;
    let app = app(Vec::new());

    for (form, location) in [
        ("theme=light", "/"),
        ("theme=light&next=", "/"),
        ("theme=light&next=https://evil.example/", "/"),
        ("theme=light&next=//evil.example/", "/"),
        ("theme=light&next=/\\evil.example", "/"),
        ("theme=light&next=/search?q=x", "/search?q=x"),
    ] {
        let r = send(&app.router, post_form("/theme", form)).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{form}");
        assert_eq!(r.header("location"), location, "{form}");
        assert!(r.header("set-cookie").starts_with("theme=light;"), "{form}");
    }
}

#[tokio::test]
async fn bad_theme_choices_are_refused() {
    let _serial = serial().await;
    let app = app(Vec::new());

    // Unknown values and unreadable bodies keep the current (dark) theme.
    for body in ["theme=pink&next=/", "theme=&next=/", "next=/", "not-a-form"] {
        let r = send(&app.router, post_form("/theme", body)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}");
        assert_dark(&r);
        assert!(r.headers.get("set-cookie").is_none(), "{body}");
        assert_eq!(r.header("vary"), "Cookie");
        assert_security_headers(&r, true);
        assert_safe_html(&r.body);
    }

    // The switch endpoint takes no GET.
    let r = send(&app.router, get("/theme")).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_security_headers(&r, true);
}

#[tokio::test]
async fn stored_theme_applies_to_every_page() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "light themed torrent", &[("a.bin", 8)])]);
    let cookie = "theme=light";

    let home = send(&app.router, get_with_cookie("/", cookie)).await;
    assert_eq!(home.status, StatusCode::OK);
    assert_light(&home);

    let key = key_for(1).to_hex();
    let detail = send(&app.router, get_with_cookie(&format!("/t/{key}"), cookie)).await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_light(&detail);
    assert!(detail.body.contains(r#"<input type="hidden" name="next""#));

    let missing = send(
        &app.router,
        get_with_cookie(&format!("/t/{}", key_for(77).to_hex()), cookie),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_light(&missing);
    assert_eq!(missing.header("vary"), "Cookie");
    assert_safe_html(&missing.body);

    // Unknown cookies and values keep the dark default.
    let dark = send(&app.router, get_with_cookie("/", "other=1")).await;
    assert_dark(&dark);
    let dark = send(&app.router, get_with_cookie("/", "theme=pink")).await;
    assert_dark(&dark);
}
