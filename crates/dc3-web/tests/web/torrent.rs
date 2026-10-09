//! `GET /t/{key}`.

use std::sync::atomic::Ordering;

use axum::http::StatusCode;

use crate::common::*;

#[tokio::test]
async fn detail_page_shows_everything() {
    let _serial = serial().await;
    let mut t = hybrid(5, "Big Buck Bunny 1080p");
    t.files = vec![
        dc3_store::FileRow {
            path: "Big Buck Bunny/movie.mkv".into(),
            size: 1_234_567_890,
        },
        dc3_store::FileRow {
            path: "Big Buck Bunny/readme.txt".into(),
            size: 512,
        },
    ];
    t.file_count = 2;
    t.total_size = 1_234_568_402;
    t.seen_count = 12_345;
    let app = app(vec![t]);
    let v1 = key_for(5).to_hex();
    let v2 = v2_for(5).to_hex();

    let r = send(&app.router, get(&format!("/t/{v1}"))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("cache-control"), "no-store");
    assert_security_headers(&r, true);
    assert_safe_html(&r.body);
    let body = &r.body;
    assert!(body.contains("<title>Big Buck Bunny 1080p · "));
    assert!(body.contains(
        r#"<h1 class="torrent-name" id="torrent-heading"><bdi>Big Buck Bunny 1080p</bdi></h1>"#
    ));
    assert!(body.contains(&format!("<dt>DHT key</dt><dd><code>{v1}</code></dd>")));
    assert!(body.contains(&format!(
        "<dt>Info hash (v1)</dt><dd><code>{v1}</code></dd>"
    )));
    assert!(body.contains(&format!(
        "<dt>Info hash (v2)</dt><dd><code>{v2}</code></dd>"
    )));
    assert!(
        body.contains(r#"<span title="1,234,568,402 bytes">1.1 GiB</span> (1,234,568,402 bytes)"#)
    );
    assert!(body.contains("<dt>Files</dt><dd>2</dd>"));
    assert!(body.contains(
        r#"<dt>First seen</dt><dd><time datetime="2026-09-01T12:30:00Z">2026-09-01</time></dd>"#
    ));
    assert!(body.contains(
        r#"<dt>Last seen</dt><dd><time datetime="2026-09-15T12:30:00Z">2026-09-15</time></dd>"#
    ));
    assert!(body.contains("<dt>Times seen</dt><dd>12,345</dd>"));
    assert!(body.contains(&format!(
        r#"<a class="button" href="magnet:?xt=urn:btih:{v1}&#38;xt=urn:btmh:1220{v2}&#38;dn=Big%20Buck%20Bunny%201080p">"#
    )));
    assert!(body.contains(r#"<td class="path"><bdi>Big Buck Bunny/movie.mkv</bdi></td><td class="num"><span title="1,234,567,890 bytes">1.1 GiB</span></td>"#));
    assert!(body.contains(r#"<span title="512 bytes">512 B</span>"#));
    assert!(!body.contains("are listed."));

    // The same torrent by its v2 hash, and by its key in upper case and base32.
    let by_v2 = send(&app.router, get(&format!("/t/{v2}"))).await;
    assert_eq!(by_v2.status, StatusCode::OK);
    // Identical apart from the theme switch's return address, which echoes
    // the requested URL (`/t/{v2}` only appears there).
    assert!(by_v2.body.contains(&format!(
        r#"<input type="hidden" name="next" value="/t/{v2}">"#
    )));
    assert_eq!(
        by_v2.body.replace(&format!("/t/{v2}"), &format!("/t/{v1}")),
        r.body
    );
    let upper = send(&app.router, get(&format!("/t/{}", v1.to_uppercase()))).await;
    assert_eq!(upper.status, StatusCode::OK);
    let b32 = data_encoding_base32(&key_for(5).0);
    let by_b32 = send(&app.router, get(&format!("/t/{b32}"))).await;
    assert_eq!(by_b32.status, StatusCode::OK);
}

/// RFC 4648 base32 without padding (the magnet-link form of a key).
fn data_encoding_base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[tokio::test]
async fn truncated_file_lists_say_so() {
    let _serial = serial().await;
    let mut t = torrent(6, "many files", &[("one", 1), ("two", 2)]);
    t.file_count = 250_000;
    t.files_truncated = true;
    let app = app(vec![t]);
    let r = send(&app.router, get(&format!("/t/{}", key_for(6).to_hex()))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains("Only the first 2 of 250,000 files are listed.")
    );
    assert!(r.body.contains("<dt>Files</dt><dd>250,000</dd>"));
}

#[tokio::test]
async fn torrent_without_files_or_magnet_parts() {
    let _serial = serial().await;
    let mut t = torrent(8, "no list", &[]);
    t.info_hash_v1 = None;
    t.info_hash_v2 = Some(v2_for(8));
    let app = app(vec![t]);
    let r = send(&app.router, get(&format!("/t/{}", key_for(8).to_hex()))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("No file list is stored for this torrent."));
    assert!(!r.body.contains("Info hash (v1)"));
    assert!(r.body.contains(&format!(
        "magnet:?xt=urn:btmh:1220{}&#38;dn=no%20list",
        v2_for(8).to_hex()
    )));
}

#[tokio::test]
async fn invalid_keys_give_400() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "x", &[])]);
    let calls = app.backend.get_by_key_calls();
    let keys = [
        "abc".to_owned(),
        "z".repeat(40),
        "0123456789abcdef0123456789abcdef0123456".to_owned(),
        "0123456789abcdef0123456789abcdef012345678".to_owned(),
        "%3Cscript%3E".to_owned(),
        "%FF%FE".to_owned(),
        "1".repeat(32),
        "a".repeat(65),
        "g".repeat(64),
    ];
    for key in &keys {
        let r = send(&app.router, get(&format!("/t/{key}"))).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{key}");
        assert!(r.body.contains("That is not a valid torrent key."), "{key}");
        assert_security_headers(&r, true);
        assert_safe_html(&r.body);
        let api = send(&app.router, get(&format!("/api/v1/torrents/{key}"))).await;
        assert_eq!(api.status, StatusCode::BAD_REQUEST, "{key}");
        assert!(api.json()["error"].is_string());
    }
    assert_eq!(app.backend.get_by_key_calls(), calls);
}

#[tokio::test]
async fn unknown_hidden_and_failing_lookups() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "visible", &[]), torrent(2, "hidden", &[])]);
    app.backend.hide(2);
    let r = send(&app.router, get(&format!("/t/{}", key_for(99).to_hex()))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(r.body.contains("No torrent with this key is available."));
    assert_security_headers(&r, true);
    let r = send(&app.router, get(&format!("/t/{}", key_for(2).to_hex()))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = send(&app.router, get(&format!("/t/{}", v2_for(1).to_hex()))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    app.backend.0.reads_fail.store(true, Ordering::SeqCst);
    let r = send(&app.router, get(&format!("/t/{}", key_for(1).to_hex()))).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_security_headers(&r, true);
}
