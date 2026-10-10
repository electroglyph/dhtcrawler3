//! `GET /search`: results, pagination and errors.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use dc4_store::TorrentRecord;

use crate::common::*;

/// Torrents named "common item N"; higher ids were seen more often, so
/// relevance puts them first.
fn many(n: i64) -> Vec<TorrentRecord> {
    (1..=n)
        .map(|id| {
            let mut t = torrent(id, &format!("common item {id}"), &[("f.txt", 100)]);
            t.seen_count = u64::try_from(id).unwrap() * 10;
            t.total_size = u64::try_from(id).unwrap();
            t
        })
        .collect()
}

fn result_names(body: &str) -> Vec<String> {
    body.split("<h2 class=\"result-title\">")
        .skip(1)
        .map(|chunk| {
            let start = chunk.find("<bdi>").unwrap() + "<bdi>".len();
            let end = chunk.find("</bdi>").unwrap();
            chunk[start..end].to_owned()
        })
        .collect()
}

#[tokio::test]
async fn results_follow_the_index_order_and_skip_missing_hits() {
    let _serial = serial().await;
    let torrents = vec![
        torrent(
            1,
            "Ubuntu 26.04 desktop amd64",
            &[("ubuntu.iso", 5_000_000_000)],
        ),
        torrent(2, "Debian netinst", &[("debian.iso", 700_000_000)]),
        torrent(3, "ubuntu server", &[("server.iso", 2_000)]),
        torrent(4, "ubuntu hidden", &[("x", 1)]),
    ];
    let app = app(torrents);
    // Torrent 4 is in the index but no longer visible in the database.
    app.backend.hide(4);
    let searches = metrics().histogram_count("dc4_search_seconds");

    let r = send(&app.router, get("/search?q=ubuntu&sort=size")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("cache-control"), "no-store");
    assert_safe_html(&r.body);
    assert_eq!(
        result_names(&r.body),
        ["Ubuntu 26.04 desktop amd64", "ubuntu server"]
    );
    // The total counts visible matches, so the hidden hit is subtracted.
    assert!(r.body.contains("2 torrents match your search."));
    assert!(
        r.body
            .contains(r#"<span title="5,000,000,000 bytes">4.7 GiB</span>"#)
    );
    assert!(r.body.contains("<span>1 file</span>"));
    assert!(
        r.body
            .contains(r#"<time datetime="2026-09-01T12:30:00Z">2026-09-01</time>"#)
    );
    assert!(r.body.contains("<span>Seen 7 times</span>"));
    let key = key_for(1).to_hex();
    assert!(r.body.contains(&format!(r#"<a href="/t/{key}">"#)));
    assert!(r.body.contains(&format!(
        r#"<a href="magnet:?xt=urn:btih:{key}&#38;dn=Ubuntu%2026.04%20desktop%20amd64">"#
    )));
    // The current sort is marked, the others are links.
    assert!(
        r.body
            .contains(r#"<span aria-current="true">Largest</span>"#)
    );
    assert!(r.body.contains(
        r#"<a href="/search?q=ubuntu&#38;p=1&#38;sort=newest&#38;per_page=20">Newest</a>"#
    ));
    // The query is shown in the heading and the search box.
    assert!(r.body.contains("Results for <bdi>ubuntu</bdi>"));
    assert!(r.body.contains(r#"value="ubuntu""#));
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 1
    );
    assert_eq!(app.backend.get_many_calls(), 1);
}

#[tokio::test]
async fn heavily_filtered_page_still_links_to_next_page() {
    // F-16: pagination follows the index count, not the page-adjusted
    // visible count. Page 1 hides both its hits, but page 2 still has
    // visible hits, so a next link must be offered.
    let _serial = serial().await;
    let app = app(many(4));
    // Relevance puts higher ids first, so page 1 (per_page=2) holds 4, 3.
    app.backend.hide(4);
    app.backend.hide(3);
    let r = send(&app.router, get("/search?q=common&per_page=2")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(result_names(&r.body).len(), 0);
    // Visible total on this page is 4 - 2, but the next page exists.
    assert!(r.body.contains("2 torrents match your search."));
    assert!(r.body.contains("rel=\"next\""));
    let r = send(&app.router, get("/search?q=common&per_page=2&p=2")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(result_names(&r.body), ["common item 2", "common item 1"]);
}

#[tokio::test]
async fn no_results() {
    let _serial = serial().await;
    let app = app(many(3));
    let r = send(&app.router, get("/search?q=nothingmatches")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("No torrents match your search."));
    assert!(
        r.body
            .contains("No torrents on this page match your search.")
    );
    assert!(!r.body.contains("rel=\"next\""));
    // Nothing to load, so the database is not asked.
    assert_eq!(app.backend.get_many_calls(), 0);
}

#[tokio::test]
async fn pagination_links_are_percent_encoded() {
    let _serial = serial().await;
    let app = app(many(45));
    // A quoted phrase plus excluded words full of URL metacharacters.
    let q = "\"common item\" -zz&zz -a#b+c%d/e?f=g -ünï";
    let encoded = "%22common%20item%22%20-zz%26zz%20-a%23b%2Bc%25d%2Fe%3Ff%3Dg%20-%C3%BCn%C3%AF";

    let r = send(&app.router, get(&format!("/search?q={}", enc(q)))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_safe_html(&r.body);
    assert!(r.body.contains("45 torrents match your search."));
    assert_eq!(result_names(&r.body).len(), 20);
    assert_eq!(result_names(&r.body)[0], "common item 45");
    assert!(r.body.contains(r#"<ol class="results" start="1">"#));
    assert!(r.body.contains(&format!(
        r#"<a rel="next" href="/search?q={encoded}&#38;p=2&#38;sort=relevance&#38;per_page=20">"#
    )));
    assert!(!r.body.contains("rel=\"prev\""));
    // No href carries the raw query. In-page anchors start with '#'.
    for part in r.body.split("href=\"").skip(1) {
        let href = part[..part.find('"').unwrap()].replace("&#38;", "&");
        assert!(!href.contains(' '), "{href}");
        assert!(!href.contains("&#"), "{href}");
        assert!(!href.contains('#') || href.starts_with('#'), "{href}");
        assert!(!href.contains("zz&zz"), "{href}");
    }

    let r = send(&app.router, get(&format!("/search?q={encoded}&p=3"))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(result_names(&r.body).len(), 5);
    assert!(r.body.contains(r#"<ol class="results" start="41">"#));
    assert!(r.body.contains(&format!(
        r#"<a rel="prev" href="/search?q={encoded}&#38;p=2&#38;sort=relevance&#38;per_page=20">"#
    )));
    assert!(!r.body.contains("rel=\"next\""));
    assert!(r.body.contains("<span>Page 3</span>"));

    // A page past the results is empty but valid.
    let r = send(&app.router, get(&format!("/search?q={encoded}&p=50"))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains("No torrents on this page match your search.")
    );
}

#[tokio::test]
async fn last_allowed_page_has_no_next_link() {
    let _serial = serial().await;
    // 50 pages of 20 are 1000 results; 1001 would need page 51.
    let app = app(many(1001));
    let r = send(&app.router, get("/search?q=common&p=50")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(result_names(&r.body).len(), 20);
    assert!(r.body.contains("1,001 torrents match your search."));
    assert!(!r.body.contains("rel=\"next\""));
    assert!(r.body.contains("rel=\"prev\""));
}

#[tokio::test]
async fn every_query_searches_normally() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz movie", &[("a", 1)])]);
    let searches = metrics().histogram_count("dc4_search_seconds");

    for q in ["zzforbiddenzz", "movie ZZFORBIDDENZZ", "bad phrase"] {
        let r = send(&app.router, get(&format!("/search?q={}", enc(q)))).await;
        assert_eq!(r.status, StatusCode::OK, "{q}");
        assert_security_headers(&r, true);
        assert_safe_html(&r.body);
        // The query is echoed back in the heading and the search box.
        assert!(r.body.contains("Results for"), "{q}");
    }
    // The first three queries match the torrent; the last matches nothing.
    let r = send(&app.router, get("/search?q=zzforbiddenzz")).await;
    assert!(r.body.contains("1 torrent matches your search."));
    assert_eq!(result_names(&r.body), ["zzforbiddenzz movie"]);
    let r = send(
        &app.router,
        get("/api/v1/search?q=zzforbiddenzz&per_page=5"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let json = r.json();
    assert_eq!(json["total"], 1);
    assert_eq!(json["results"].as_array().unwrap().len(), 1);
    assert_eq!(json["query"], "zzforbiddenzz");
    assert_eq!(json["per_page"], 5);
    // The no-match query never touched the database; the rest did.
    // Hydration runs on every request, cached or not.
    assert_eq!(app.backend.get_many_calls(), 4);
    // Five requests but four index searches: the repeated HTML query is
    // served from the search cache (the API request differs by per_page,
    // and the empty query is never inserted).
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 4
    );

    // Bad parameters still answer 400.
    for bad in [
        "/api/v1/search?q=hello&p=0",
        "/api/v1/search?q=hello&per_page=999",
        "/api/v1/search?q=hello&sort=bogus",
    ] {
        let r = send(&app.router, get(bad)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn short_prefixes_search_file_names() {
    let _serial = serial().await;
    // The term is only in the files: reachable through prefix expansion.
    let app = app(vec![torrent(
        1,
        "holiday photos",
        &[("zzforbiddenzz/a.jpg", 1)],
    )]);
    let searches = metrics().histogram_count("dc4_search_seconds");
    for q in ["zz", "zzforbiddenz"] {
        let r = send(&app.router, get(&format!("/search?q={}", enc(q)))).await;
        assert_eq!(r.status, StatusCode::OK, "{q}");
        assert_eq!(result_names(&r.body), ["holiday photos"], "{q}");
        let r = send(&app.router, get(&format!("/api/v1/search?q={}", enc(q)))).await;
        assert_eq!(r.status, StatusCode::OK, "{q}");
        let json = r.json();
        assert_eq!(json["total"], 1, "{q}");
        assert_eq!(json["query"], q, "{q}");
    }
    // Two index searches for four requests: each API request shares its
    // HTML request's cache entry (same text, sort, page and per_page).
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 2
    );
}

#[tokio::test]
async fn over_long_queries_are_refused_before_searching() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz movie", &[("a", 1)])]);
    // Over the length limit: refused as too long, never searched.
    let q = format!("zzforbiddenzz {}", "\u{FDFA}".repeat(250));
    let r = send(&app.router, get(&format!("/search?q={}", enc(&q)))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("Your search is too long."));
    // The refused query is not echoed back.
    assert!(!r.body.to_lowercase().contains("zzf"));
    let r = send(&app.router, get(&format!("/api/v1/search?q={}", enc(&q)))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(!r.body.to_lowercase().contains("zzf"));
    assert_eq!(app.backend.get_many_calls(), 0);
}

#[tokio::test]
async fn every_stored_torrent_is_shown() {
    let _serial = serial().await;
    let app = app(vec![
        torrent(1, "holiday photos", &[("zzforbiddenzz/a.jpg", 1)]),
        torrent(2, "holiday video", &[("b.mp4", 1)]),
        torrent(3, "holiday zzforbiddenzz", &[("c.mp4", 1)]),
    ]);
    // Result lists show every visible hit, whatever the stored text holds.
    let r = send(&app.router, get("/search?q=holiday")).await;
    let mut names = result_names(&r.body);
    names.sort();
    assert_eq!(
        names,
        ["holiday photos", "holiday video", "holiday zzforbiddenzz"]
    );
    let r = send(&app.router, get("/api/v1/search?q=holiday")).await;
    assert_eq!(r.json()["results"].as_array().unwrap().len(), 3);
    // Detail pages show the name and every listed path.
    for id in [1, 2, 3] {
        let key = key_for(id).to_hex();
        let r = send(&app.router, get(&format!("/t/{key}"))).await;
        assert_eq!(r.status, StatusCode::OK, "{id}");
        let r = send(&app.router, get(&format!("/api/v1/torrents/{key}"))).await;
        assert_eq!(r.status, StatusCode::OK, "{id}");
    }
}

#[tokio::test]
async fn bad_parameters_give_friendly_400s() {
    let _serial = serial().await;
    let app = app(many(3));
    for (uri, needle) in [
        (
            "/search?q=common&p=0",
            "The page number must be between 1 and 50.",
        ),
        (
            "/search?q=common&p=51",
            "The page number must be between 1 and 50.",
        ),
        (
            "/search?q=common&p=x",
            "The page number must be between 1 and 50.",
        ),
        ("/search?q=common&sort=random", "The sort order must be"),
        ("/search?q=-common", "Your search has no words to look for."),
        ("/search?q=a+b+c+d+e+f+g+h+i+j+k+l+m", "Use at most 12."),
        ("/search?q=common&q=other", "could not be read"),
    ] {
        let r = send(&app.router, get(uri)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(r.body.contains(needle), "{uri}: {}", r.body);
        assert_safe_html(&r.body);
        assert_security_headers(&r, true);
    }
    let long = "x".repeat(201);
    let r = send(&app.router, get(&format!("/search?q={long}"))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("Use at most 200 characters."));
    // 200 characters (of any script) are fine.
    let ok = "東".repeat(200);
    let r = send(&app.router, get(&format!("/search?q={}", enc(&ok)))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // Surrounding whitespace does not count: ten words, 200 characters.
    let words = format!("{} {}", "y".repeat(20), vec!["y".repeat(19); 9].join(" "));
    assert_eq!(words.chars().count(), 200);
    let padded = format!("  {words}\t ");
    let r = send(&app.router, get(&format!("/search?q={}", enc(&padded)))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // A single word longer than a token is not searchable.
    let r = send(&app.router, get(&format!("/search?q={}", "y".repeat(65)))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("Your search has no words to look for."));
}

#[tokio::test]
async fn empty_query_redirects_home() {
    let _serial = serial().await;
    let app = app(Vec::new());
    for uri in ["/search", "/search?q=", "/search?q=%20%09", "/search?p=2"] {
        let r = send(&app.router, get(uri)).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(r.header("location"), "/");
        assert_security_headers(&r, true);
    }
}

#[tokio::test]
async fn database_failure_while_loading_results_gives_503() {
    let _serial = serial().await;
    let app = app(many(3));
    app.backend.0.reads_fail.store(true, Ordering::SeqCst);
    let r = send(&app.router, get("/search?q=common")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.body.contains("The database is not available right now."));
    assert!(r.body.contains(r#"value="common""#));
    assert_security_headers(&r, true);
    let r = send(&app.router, get("/api/v1/search?q=common")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.json()["error"].is_string());
}

#[tokio::test]
async fn cjk_and_prefix_searches() {
    let _serial = serial().await;
    let app = app(vec![
        torrent(1, "東京大学 講義", &[("第一回.mp4", 1)]),
        torrent(2, "Photography basics", &[("lesson.mkv", 1)]),
    ]);
    let r = send(&app.router, get(&format!("/search?q={}", enc("京大")))).await;
    assert_eq!(result_names(&r.body), ["東京大学 講義"]);
    let r = send(&app.router, get(&format!("/search?q={}", enc("回")))).await;
    assert_eq!(result_names(&r.body), ["東京大学 講義"]);
    let r = send(&app.router, get("/search?q=photog")).await;
    assert_eq!(result_names(&r.body), ["Photography basics"]);
}

// ------------------------------------------------------- search query cache

#[tokio::test]
async fn repeated_searches_hit_the_cache() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz movie", &[("a", 1)])]);
    let searches = metrics().histogram_count("dc4_search_seconds");
    let hits = metrics().counter("dc4_search_cache_hits_total");
    let misses = metrics().counter("dc4_search_cache_misses_total{reason=absent}");

    // HTML first: a miss that searches the index once ...
    let r = send(&app.router, get("/search?q=zzforbiddenzz")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("cache-control"), "no-store");
    assert_eq!(result_names(&r.body), ["zzforbiddenzz movie"]);
    // ... then the API shares the entry: same text, sort, page and per_page.
    let r = send(&app.router, get("/api/v1/search?q=zzforbiddenzz")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("cache-control"), "no-store");
    assert_eq!(r.json()["total"], 1);
    // Repeat HTML: a hit, so no index search is recorded.
    let r = send(&app.router, get("/search?q=zzforbiddenzz")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("cache-control"), "no-store");

    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 1
    );
    assert_eq!(metrics().counter("dc4_search_cache_hits_total"), hits + 2);
    assert_eq!(
        metrics().counter("dc4_search_cache_misses_total{reason=absent}"),
        misses + 1
    );
    // Hydration still runs on every request, cached or not.
    assert_eq!(app.backend.get_many_calls(), 3);
}

#[tokio::test]
async fn distinct_pages_and_sorts_search_again() {
    let _serial = serial().await;
    let app = app(many(45));
    let searches = metrics().histogram_count("dc4_search_seconds");
    let hits = metrics().counter("dc4_search_cache_hits_total");

    for uri in [
        "/search?q=common",
        "/search?q=common&p=2",
        "/search?q=common&sort=newest",
    ] {
        let r = send(&app.router, get(uri)).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
    }
    // Page and sort variants are separate index queries, so all three
    // searched; repeating page 1 is a hit, not a fourth search.
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 3
    );
    let r = send(&app.router, get("/search?q=common")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 3
    );
    assert_eq!(metrics().counter("dc4_search_cache_hits_total"), hits + 1);
}

#[tokio::test]
async fn empty_searches_are_never_cached() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz movie", &[("a", 1)])]);
    let searches = metrics().histogram_count("dc4_search_seconds");
    let hits = metrics().counter("dc4_search_cache_hits_total");
    let misses = metrics().counter("dc4_search_cache_misses_total{reason=absent}");

    for _ in 0..2 {
        let r = send(&app.router, get("/search?q=nothingmatches")).await;
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.contains("No torrents match your search."));
    }
    // Both runs searched the index: the empty outcome is shared by no
    // flight and inserted nowhere, so the repeat is a miss, not a hit.
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 2
    );
    assert_eq!(metrics().counter("dc4_search_cache_hits_total"), hits);
    assert_eq!(
        metrics().counter("dc4_search_cache_misses_total{reason=absent}"),
        misses + 2
    );
    assert_eq!(app.backend.get_many_calls(), 0);
}

#[tokio::test]
async fn commit_between_requests_invalidates_the_cache() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz one", &[("a", 1)])]);
    let searches = metrics().histogram_count("dc4_search_seconds");
    let hits = metrics().counter("dc4_search_cache_hits_total");
    let stamps = metrics().counter("dc4_search_cache_misses_total{reason=stamp}");

    let r = send(&app.router, get("/search?q=zzforbiddenzz")).await;
    assert!(r.body.contains("1 torrent matches your search."));
    // A repeat is a hit: still one index search.
    let r = send(&app.router, get("/search?q=zzforbiddenzz")).await;
    assert!(r.body.contains("1 torrent matches your search."));
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 1
    );

    // A commit changes the index stamp, so the whole cache clears: the next
    // request searches again and sees the new torrent.
    app.commit_torrent(torrent(2, "zzforbiddenzz two", &[("b", 2)]));
    let r = send(&app.router, get("/search?q=zzforbiddenzz")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("2 torrents match your search."));
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 2
    );
    assert_eq!(metrics().counter("dc4_search_cache_hits_total"), hits + 1);
    assert_eq!(
        metrics().counter("dc4_search_cache_misses_total{reason=stamp}"),
        stamps + 1
    );
}

#[tokio::test]
async fn concurrent_identical_misses_share_one_search() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz movie", &[("a", 1)])]);
    let searches = metrics().histogram_count("dc4_search_seconds");
    let misses = metrics().counter("dc4_search_cache_misses_total{reason=absent}");
    let hits = metrics().counter("dc4_search_cache_hits_total");
    let coalesced = metrics().counter("dc4_search_cache_coalesced_total");

    const N: usize = 16;
    let gate = Arc::new(tokio::sync::Barrier::new(N));
    let mut tasks = Vec::with_capacity(N);
    for _ in 0..N {
        let router = app.router.clone();
        let gate = Arc::clone(&gate);
        tasks.push(tokio::spawn(async move {
            gate.wait().await;
            send(&router, get("/search?q=zzforbiddenzz")).await
        }));
    }
    for task in tasks {
        let r = task.await.unwrap();
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(result_names(&r.body), ["zzforbiddenzz movie"]);
    }
    // Exactly one index search served all sixteen requests. Every
    // non-hit lookup counts a miss — the leader's and each waiter's — and
    // every waiter also counts coalesced; neither records index latency
    // again. A late arrival may hit the installed entry instead of waiting,
    // so the test asserts the conservation relations, not the split.
    assert_eq!(
        metrics().histogram_count("dc4_search_seconds"),
        searches + 1
    );
    let missed = metrics().counter("dc4_search_cache_misses_total{reason=absent}") - misses;
    let waited = metrics().counter("dc4_search_cache_coalesced_total") - coalesced;
    let hit = metrics().counter("dc4_search_cache_hits_total") - hits;
    assert_eq!(missed, waited + 1);
    assert_eq!(hit + waited + 1, N as u64);
}
