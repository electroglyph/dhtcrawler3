//! The JSON API.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::Value;

use crate::common::*;

const TORRENT_FIELDS: [&str; 10] = [
    "dht_key",
    "info_hash_v1",
    "info_hash_v2",
    "name",
    "size",
    "file_count",
    "first_seen",
    "last_seen",
    "seen_count",
    "magnet",
];

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn search_json_has_exactly_the_documented_fields() {
    let _serial = serial().await;
    let mut hidden = torrent(3, "api example three", &[("c", 3)]);
    hidden.deleted_at = Some(at(2));
    let app = app(vec![
        hybrid(1, "api example one"),
        torrent(2, "api example two", &[("b", 2)]),
        hidden,
    ]);
    let r = send(
        &app.router,
        get("/api/v1/search?q=example&per_page=3&sort=newest"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), "application/json");
    assert_eq!(r.header("cache-control"), "no-store");
    assert_security_headers(&r, true);
    let json = r.json();
    assert_eq!(
        keys(&json),
        set(&["query", "page", "per_page", "total", "results"])
    );
    assert_eq!(json["query"], "example");
    assert_eq!(json["page"], 1);
    assert_eq!(json["per_page"], 3);
    assert_eq!(json["total"], 2);
    let results = json["results"].as_array().unwrap();
    // The hidden hit is skipped, and the total counts visible matches only.
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(keys(result), set(&TORRENT_FIELDS));
    }
    let one = results
        .iter()
        .find(|t| t["name"] == "api example one")
        .unwrap();
    let v1 = key_for(1).to_hex();
    let v2 = v2_for(1).to_hex();
    assert_eq!(one["dht_key"], v1.as_str());
    assert_eq!(one["info_hash_v1"], v1.as_str());
    assert_eq!(one["info_hash_v2"], v2.as_str());
    assert_eq!(one["size"], 2048);
    assert_eq!(one["file_count"], 1);
    assert_eq!(one["first_seen"], "2026-09-01T12:30:00Z");
    assert_eq!(one["last_seen"], "2026-09-15T12:30:00Z");
    assert_eq!(one["seen_count"], 7);
    assert_eq!(
        one["magnet"],
        format!("magnet:?xt=urn:btih:{v1}&xt=urn:btmh:1220{v2}&dn=api%20example%20one")
    );
    let text = r.body.as_str();
    for field in [
        "change_seq",
        "hidden_at",
        "deleted_at",
        "reviewed_at",
        "piece_length",
        "\"id\"",
    ] {
        assert!(!text.contains(field), "{field}");
    }
}

#[tokio::test]
async fn torrent_json_has_exactly_the_documented_fields() {
    let _serial = serial().await;
    let mut t = torrent(4, "api detail", &[("dir/a.txt", 10), ("dir/b.txt", 20)]);
    t.files_truncated = true;
    t.file_count = 5000;
    let app = app(vec![t]);
    let key = key_for(4).to_hex();
    let r = send(&app.router, get(&format!("/api/v1/torrents/{key}"))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), "application/json");
    assert_security_headers(&r, true);
    let json = r.json();
    let mut expected = set(&TORRENT_FIELDS);
    expected.insert("files".into());
    expected.insert("files_truncated".into());
    assert_eq!(keys(&json), expected);
    assert_eq!(json["dht_key"], key.as_str());
    assert_eq!(json["info_hash_v2"], Value::Null);
    assert_eq!(json["file_count"], 5000);
    assert_eq!(json["files_truncated"], true);
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    for file in files {
        assert_eq!(keys(file), set(&["path", "size"]));
    }
    assert_eq!(files[0]["path"], "dir/a.txt");
    assert_eq!(files[0]["size"], 10);
    for field in [
        "change_seq",
        "hidden_at",
        "deleted_at",
        "reviewed_at",
        "piece_length",
    ] {
        assert!(!r.body.contains(field), "{field}");
    }
    // Compact output.
    assert!(!r.body.contains('\n'));
}

#[tokio::test]
async fn torrent_json_flags_unflagged_short_file_list() {
    let _serial = serial().await;
    // Neither the store flag nor the display budget fires, but the store
    // hands back fewer files than `file_count`: still truncated.
    let mut t = torrent(5, "short list", &[("dir/a.txt", 10), ("dir/b.txt", 20)]);
    t.file_count = 5;
    assert!(!t.files_truncated);
    let app = app(vec![t]);
    let r = send(
        &app.router,
        get(&format!("/api/v1/torrents/{}", key_for(5).to_hex())),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let json = r.json();
    assert_eq!(json["files"].as_array().unwrap().len(), 2);
    assert_eq!(json["files_truncated"], true);
}

#[tokio::test]
async fn api_errors_are_json() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "api error case", &[])]);
    for (uri, status) in [
        ("/api/v1/search", StatusCode::BAD_REQUEST),
        ("/api/v1/search?q=%20", StatusCode::BAD_REQUEST),
        ("/api/v1/search?q=a&per_page=51", StatusCode::BAD_REQUEST),
        ("/api/v1/search?q=a&per_page=0", StatusCode::BAD_REQUEST),
        ("/api/v1/search?q=a&p=51", StatusCode::BAD_REQUEST),
        ("/api/v1/search?q=a&sort=best", StatusCode::BAD_REQUEST),
        ("/api/v1/search?q=-a", StatusCode::BAD_REQUEST),
        ("/api/v1/torrents/xyz", StatusCode::BAD_REQUEST),
        (
            "/api/v1/torrents/0000000000000000000000000000000000000000",
            StatusCode::NOT_FOUND,
        ),
        ("/api/v1/nothing", StatusCode::NOT_FOUND),
        ("/api", StatusCode::NOT_FOUND),
    ] {
        let r = send(&app.router, get(uri)).await;
        assert_eq!(r.status, status, "{uri}");
        let json = r.json();
        assert_eq!(keys(&json), set(&["error"]), "{uri}");
        assert!(!json["error"].as_str().unwrap().is_empty());
        assert_security_headers(&r, true);
    }
    let long = "x".repeat(201);
    let r = send(&app.router, get(&format!("/api/v1/search?q={long}"))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        r.json()["error"],
        "Your search is too long. Use at most 200 characters."
    );
}

#[tokio::test]
async fn api_pages_and_sizes() {
    let _serial = serial().await;
    let torrents: Vec<_> = (1..=60)
        .map(|id| torrent(id, &format!("paged entry {id}"), &[("x", 1)]))
        .collect();
    let app = app(torrents);
    let r = send(&app.router, get("/api/v1/search?q=paged&per_page=50")).await;
    let json = r.json();
    assert_eq!(json["total"], 60);
    assert_eq!(json["results"].as_array().unwrap().len(), 50);
    let r = send(&app.router, get("/api/v1/search?q=paged&per_page=50&p=2")).await;
    let json = r.json();
    assert_eq!(json["page"], 2);
    assert_eq!(json["results"].as_array().unwrap().len(), 10);
    let r = send(&app.router, get("/api/v1/search?q=paged")).await;
    assert_eq!(r.json()["per_page"], 20);
    assert_eq!(r.json()["results"].as_array().unwrap().len(), 20);
}
