//! `GET /report/{key}` and `POST /report/{key}`.

use axum::body::Body;
use axum::http::{Method, StatusCode};
use dc3_core::AnyKey;
use dc3_store::ReportReason;

use crate::common::*;

fn form(reason: &str, message: &str, contact: &str) -> String {
    format!(
        "reason={}&message={}&contact={}",
        enc(reason),
        enc(message),
        enc(contact)
    )
}

#[tokio::test]
async fn report_form_is_accessible_and_not_cached() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let r = send(&app.router, get(&format!("/report/{}", key.to_uppercase()))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("cache-control"), "no-store");
    assert_security_headers(&r, true);
    assert_safe_html(&r.body);
    let body = &r.body;
    assert!(body.contains(&format!(
        r#"<form class="report-form" action="/report/{key}" method="post">"#
    )));
    for (value, label) in [
        ("csam", "Child sexual abuse material"),
        ("copyright", "Copyright infringement"),
        ("malware", "Malware or a harmful file"),
        ("other", "Something else"),
    ] {
        assert!(body.contains(&format!(
            r#"<input type="radio" id="reason-{value}" name="reason" value="{value}" required>"#
        )));
        assert!(body.contains(&format!(r#"<label for="reason-{value}">{label}</label>"#)));
    }
    assert!(body.contains(r#"<textarea id="message" name="message" rows="6" maxlength="2000""#));
    assert!(body.contains(r#"<input id="contact" name="contact" type="text" maxlength="320""#));
    assert!(body.contains(r#"<label for="message">"#));
    assert!(body.contains(r#"<label for="contact">"#));
    assert!(body.contains(&format!(r#"<code>{key}</code>"#)));
    // The form does not look the torrent up.
    assert_eq!(app.backend.get_by_key_calls(), 0);
}

#[tokio::test]
async fn csrf_checks() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let body = form("copyright", "mine", "");
    let cases: [(&[(&str, &str)], StatusCode); 10] = [
        (&SAME_ORIGIN_HEADERS, StatusCode::OK),
        (
            &[("sec-fetch-site", "none"), ("origin", BASE_URL)],
            StatusCode::OK,
        ),
        (&[("origin", BASE_URL)], StatusCode::OK),
        // A forged fetch-metadata header alone proves nothing.
        (&[("sec-fetch-site", "same-origin")], StatusCode::FORBIDDEN),
        (&[("sec-fetch-site", "none")], StatusCode::FORBIDDEN),
        (&[("sec-fetch-site", "cross-site")], StatusCode::FORBIDDEN),
        (
            &[("sec-fetch-site", "same-site"), ("origin", BASE_URL)],
            StatusCode::FORBIDDEN,
        ),
        (&[("origin", "https://evil.example")], StatusCode::FORBIDDEN),
        (&[("origin", "null")], StatusCode::FORBIDDEN),
        (
            &[("referer", "https://search.example.org/")],
            StatusCode::FORBIDDEN,
        ),
    ];
    let mut stored = 0;
    for (i, (headers, expected)) in cases.into_iter().enumerate() {
        // A fresh client each time keeps the report rate limit out of the way.
        let peer = format!("192.0.2.{}:1", i + 10);
        let r = send(&app.router, report_post(&key, headers, &body, &peer)).await;
        assert_eq!(r.status, expected, "{headers:?}");
        assert_security_headers(&r, true);
        assert_eq!(r.header("cache-control"), "no-store");
        if expected == StatusCode::OK {
            stored += 1;
            assert!(r.body.contains("Thank you"));
        } else {
            assert!(r.body.contains("Not allowed"));
        }
        assert_eq!(app.backend.reports().len(), stored, "{headers:?}");
    }
    // No headers at all.
    let r = send(&app.router, report_post(&key, &[], &body, "192.0.2.99:1")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(app.backend.reports().len(), stored);
}

#[tokio::test]
async fn large_valid_report_is_accepted_and_stored_exactly() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let message: String = "漢".repeat(2000);
    let contact: String = format!("{}@example.org", "c".repeat(308));
    assert_eq!(contact.chars().count(), 320);
    let body = form("malware", &message, &contact);
    assert!(
        body.len() > 18_000 && body.len() < 32 * 1024,
        "{}",
        body.len()
    );
    let r = send(
        &app.router,
        report_post(&key, &SAME_ORIGIN_HEADERS, &body, CLIENT),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let reports = app.backend.reports();
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(report.key, AnyKey::V1OrDht(key_for(1)));
    assert_eq!(report.reason, ReportReason::Malware);
    assert_eq!(report.message.chars().count(), 2000);
    assert_eq!(report.message, message);
    assert_eq!(report.contact.as_deref(), Some(contact.as_str()));
}

#[tokio::test]
async fn oversized_reports_are_refused_with_413() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let prefix = "reason=other&message=";
    let limit = 32 * 1024;
    let big = format!("{prefix}{}", "a".repeat(limit + 1 - prefix.len()));
    assert_eq!(big.len(), limit + 1);

    // With a Content-Length header.
    let r = send(
        &app.router,
        report_post(&key, &SAME_ORIGIN_HEADERS, &big, "192.0.2.50:1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_security_headers(&r, true);
    assert!(r.body.contains("too large"));

    // Streamed, without a length.
    let mut req = request(
        Method::POST,
        &format!("/report/{key}"),
        Body::from_stream(Body::from(big.clone()).into_data_stream()),
        "192.0.2.51:1",
    );
    req.headers_mut().insert(
        "content-type",
        "application/x-www-form-urlencoded".parse().unwrap(),
    );
    req.headers_mut()
        .insert("sec-fetch-site", "same-origin".parse().unwrap());
    req.headers_mut()
        .insert("origin", BASE_URL.parse().unwrap());
    let r = send(&app.router, req).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_security_headers(&r, true);
    assert!(app.backend.reports().is_empty());

    // Exactly at the limit the body is read, and the long message is
    // refused as a form error instead.
    let exact = &big[..limit];
    let r = send(
        &app.router,
        report_post(&key, &SAME_ORIGIN_HEADERS, exact, "192.0.2.52:1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.body
            .contains("The details are too long. Use at most 2000 characters.")
    );
    assert!(app.backend.reports().is_empty());
}

#[tokio::test]
async fn report_outcomes_are_counted_but_never_shown() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let m = metrics();
    let csam = "dc3_reports_total{reason=csam}";
    let copyright = "dc3_reports_total{reason=copyright}";
    let hidden = "dc3_autohide_total";
    let exhausted = "dc3_autohide_budget_exhausted_total";
    let (csam0, copyright0, hidden0, exhausted0) = (
        m.counter(csam),
        m.counter(copyright),
        m.counter(hidden),
        m.counter(exhausted),
    );

    let submit = |reason: &str, peer: &str| {
        report_post(&key, &SAME_ORIGIN_HEADERS, &form(reason, "", ""), peer)
    };

    app.backend.set_behaviour(ReportBehaviour::Hidden);
    let first = send(&app.router, submit("csam", "192.0.2.60:1")).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(m.counter(csam), csam0 + 1);
    assert_eq!(m.counter(hidden), hidden0 + 1);
    assert_eq!(m.counter(exhausted), exhausted0);

    app.backend.set_behaviour(ReportBehaviour::BudgetExhausted);
    let second = send(&app.router, submit("csam", "192.0.2.61:1")).await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(m.counter(csam), csam0 + 2);
    assert_eq!(m.counter(hidden), hidden0 + 1);
    assert_eq!(m.counter(exhausted), exhausted0 + 1);

    app.backend.set_behaviour(ReportBehaviour::Stored);
    let third = send(&app.router, submit("csam", "192.0.2.62:1")).await;
    assert_eq!(third.status, StatusCode::OK);
    assert_eq!(m.counter(csam), csam0 + 3);
    assert_eq!(m.counter(hidden), hidden0 + 1);
    assert_eq!(m.counter(exhausted), exhausted0 + 1);

    // The visitor sees the same page whatever happened.
    assert_eq!(first.body, second.body);
    assert_eq!(first.body, third.body);
    assert!(first.body.contains("Your report has been received."));
    assert!(first.body.contains("https://report.cybertip.org/"));
    assert!(!first.body.contains("has been hidden"));
    assert_safe_html(&first.body);

    let other = send(&app.router, submit("copyright", "192.0.2.63:1")).await;
    assert_eq!(other.status, StatusCode::OK);
    assert_eq!(m.counter(copyright), copyright0 + 1);
    assert!(!other.body.contains("https://report.cybertip.org/"));
    assert_eq!(app.backend.reports().len(), 4);
}

#[tokio::test]
async fn backend_refusals() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let m = metrics();
    let other = "dc3_reports_total{reason=other}";
    let before = m.counter(other);
    let body = form("other", "why", "me@example.org");

    for (i, behaviour) in [ReportBehaviour::Full, ReportBehaviour::Down]
        .into_iter()
        .enumerate()
    {
        app.backend.set_behaviour(behaviour);
        let peer = format!("192.0.2.{}:1", 70 + i);
        let r = send(
            &app.router,
            report_post(&key, &SAME_ORIGIN_HEADERS, &body, &peer),
        )
        .await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{behaviour:?}");
        assert!(r.body.contains("Reports are temporarily unavailable."));
        assert_security_headers(&r, true);
    }

    app.backend.set_behaviour(ReportBehaviour::Invalid);
    let r = send(
        &app.router,
        report_post(&key, &SAME_ORIGIN_HEADERS, &body, "192.0.2.72:1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("The report could not be accepted."));
    // The visitor's input is kept in the form.
    assert!(r.body.contains(">why</textarea>"));
    assert!(r.body.contains(r#"value="me@example.org""#));
    assert!(r.body.contains(r#"value="other" required checked>"#));
    assert_eq!(m.counter(other), before);
}

#[tokio::test]
async fn invalid_reports_are_refused_with_the_form() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let cases = [
        (form("", "text", ""), "Please choose a reason."),
        (form("spam", "text", ""), "Please choose a reason."),
        ("message=only".to_owned(), "Please choose a reason."),
        (
            form("other", &"x".repeat(2001), ""),
            "The details are too long. Use at most 2000 characters.",
        ),
        (
            form("other", "", &"c".repeat(321)),
            "The contact address is too long. Use at most 320 characters.",
        ),
    ];
    for (i, (body, message)) in cases.iter().enumerate() {
        let peer = format!("192.0.2.{}:1", 80 + i);
        let r = send(
            &app.router,
            report_post(&key, &SAME_ORIGIN_HEADERS, body, &peer),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{message}");
        assert!(r.body.contains(message), "{message}");
        assert!(r.body.contains("<form class=\"report-form\""));
        assert!(r.body.contains("role=\"alert\""));
        assert_safe_html(&r.body);
    }
    // Line breaks count once and markup is escaped when the form is shown again.
    let crlf = "<b>\r\n".repeat(400);
    let r = send(
        &app.router,
        report_post(
            &key,
            &SAME_ORIGIN_HEADERS,
            &form("x", &crlf, ""),
            "192.0.2.90:1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("&#60;b&#62;\n&#60;b&#62;"));
    assert_safe_html(&r.body);

    // Wrong content type.
    let mut req = report_post(&key, &SAME_ORIGIN_HEADERS, "{}", "192.0.2.91:1");
    req.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    let r = send(&app.router, req).await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_security_headers(&r, true);

    // A bad key.
    let r = send(
        &app.router,
        report_post(
            "nope",
            &SAME_ORIGIN_HEADERS,
            &form("other", "", ""),
            "192.0.2.92:1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(app.backend.reports().is_empty());
}

#[tokio::test]
async fn crlf_messages_are_normalised_before_counting() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    // 1000 CRLF line breaks are 2000 characters as sent, 1000 as typed.
    let message = "\r\n".repeat(999) + "end";
    let r = send(
        &app.router,
        report_post(
            &key,
            &SAME_ORIGIN_HEADERS,
            &form("other", &message, " x\u{202E}y "),
            CLIENT,
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let report = &app.backend.reports()[0];
    assert_eq!(report.message, "end");
    assert_eq!(report.contact.as_deref(), Some("xy"));

    let message = format!("a{}b", "\r\n".repeat(1500));
    let r = send(
        &app.router,
        report_post(
            &key,
            &SAME_ORIGIN_HEADERS,
            &form("other", &message, ""),
            "192.0.2.93:1",
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let report = &app.backend.reports()[1];
    assert_eq!(report.message.chars().count(), 1502);
    assert_eq!(report.contact, None);
}

#[tokio::test]
async fn report_submissions_are_rate_limited() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "reportable", &[])]);
    let key = key_for(1).to_hex();
    let limited = "dc3_rate_limited_total{route=/report/{key}}";
    let before = metrics().counter(limited);
    let body = form("other", "", "");
    let peer = "198.51.100.200:5000";
    for _ in 0..3 {
        let r = send(
            &app.router,
            report_post(&key, &SAME_ORIGIN_HEADERS, &body, peer),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    let r = send(
        &app.router,
        report_post(&key, &SAME_ORIGIN_HEADERS, &body, peer),
    )
    .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_security_headers(&r, true);
    let retry: u64 = r.header("retry-after").parse().unwrap();
    assert!((19..=20).contains(&retry), "{retry}");
    assert_eq!(metrics().counter(limited), before + 1);
    assert_eq!(app.backend.reports().len(), 3);
    // The form itself is a page and still loads.
    let form_page = send(&app.router, get_from(&format!("/report/{key}"), peer)).await;
    assert_eq!(form_page.status, StatusCode::OK);
}
