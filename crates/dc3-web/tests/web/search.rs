//! `GET /search`: results, pagination, the blocked-term policy and errors.

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use dc3_store::TorrentRecord;

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
    let searches = metrics().histogram_count("dc3_search_seconds");

    let r = send(&app.router, get("/search?q=ubuntu&sort=size")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("cache-control"), "no-store");
    assert_safe_html(&r.body);
    assert_eq!(
        result_names(&r.body),
        ["Ubuntu 26.04 desktop amd64", "ubuntu server"]
    );
    // The total comes from the index.
    assert!(r.body.contains("3 torrents match your search."));
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
    assert!(
        r.body
            .contains(r#"<a href="/search?q=ubuntu&#38;p=1&#38;sort=newest">Newest</a>"#)
    );
    // The query is shown in the heading and the search box.
    assert!(r.body.contains("Results for <bdi>ubuntu</bdi>"));
    assert!(r.body.contains(r#"value="ubuntu""#));
    assert_eq!(
        metrics().histogram_count("dc3_search_seconds"),
        searches + 1
    );
    assert_eq!(app.backend.get_many_calls(), 1);
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
        r#"<a rel="next" href="/search?q={encoded}&#38;p=2&#38;sort=relevance">"#
    )));
    assert!(!r.body.contains("rel=\"prev\""));
    // No href carries the raw query.
    for part in r.body.split("href=\"").skip(1) {
        let href = part[..part.find('"').unwrap()].replace("&#38;", "&");
        assert!(!href.contains(' '), "{href}");
        assert!(!href.contains("&#"), "{href}");
        let fragment_link = href.starts_with("/legal#") || href.starts_with('#');
        assert!(!href.contains('#') || fragment_link, "{href}");
        assert!(!href.contains("zz&zz"), "{href}");
    }

    let r = send(&app.router, get(&format!("/search?q={encoded}&p=3"))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(result_names(&r.body).len(), 5);
    assert!(r.body.contains(r#"<ol class="results" start="41">"#));
    assert!(r.body.contains(&format!(
        r#"<a rel="prev" href="/search?q={encoded}&#38;p=2&#38;sort=relevance">"#
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
async fn blocked_queries_get_the_deterrence_page_without_searching() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz movie", &[("a", 1)])]);
    let blocked = metrics().counter("dc3_blocked_queries_total");
    let searches = metrics().histogram_count("dc3_search_seconds");

    for q in [
        "zzforbiddenzz",
        "movie ZZFORBIDDENZZ",
        "zzf0rbiddenzz",
        "a Bad-Phrase",
    ] {
        let r = send(&app.router, get(&format!("/search?q={}", enc(q)))).await;
        assert_eq!(r.status, StatusCode::OK, "{q}");
        assert_security_headers(&r, true);
        assert_safe_html(&r.body);
        assert!(
            r.body
                .contains("Searches for child sexual abuse material are blocked."),
            "{q}"
        );
        assert!(
            r.body
                .contains(r#"<a href="https://www.stopitnow.org/">Stop It Now</a>"#)
        );
        assert!(
            r.body
                .contains(r#"<a href="https://report.cybertip.org/">NCMEC CyberTipline</a>"#)
        );
        assert!(
            r.body
                .contains(r#"<a href="https://report.iwf.org.uk/">IWF"#)
        );
        assert!(!r.body.contains("result-title"));
        // The query is not echoed back.
        assert!(!r.body.to_lowercase().contains("zzf"), "{q}");
        assert!(!r.body.contains("value=\"movie"));
    }
    assert_eq!(metrics().counter("dc3_blocked_queries_total"), blocked + 4);
    assert_eq!(app.backend.get_many_calls(), 0);
    assert_eq!(metrics().histogram_count("dc3_search_seconds"), searches);

    // The API gives an empty, flagged result.
    let r = send(
        &app.router,
        get("/api/v1/search?q=zzforbiddenzz&per_page=5"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let json = r.json();
    assert_eq!(json["blocked"], true);
    assert_eq!(json["total"], 0);
    assert_eq!(json["results"].as_array().unwrap().len(), 0);
    // The refused query is not echoed back.
    assert_eq!(json["query"], "");
    assert_eq!(json["per_page"], 5);
    assert_eq!(metrics().counter("dc3_blocked_queries_total"), blocked + 5);
    assert_eq!(app.backend.get_many_calls(), 0);

    // Bad parameters answer alike for blocked and clean queries: no oracle.
    for q in ["zzforbiddenzz", "hello"] {
        for bad in [
            "/api/v1/search?q=hello&p=0",
            "/api/v1/search?q=hello&per_page=999",
            "/api/v1/search?q=hello&sort=bogus",
        ] {
            let url = bad.replacen("q=hello", &format!("q={q}"), 1);
            let r = send(&app.router, get(&url)).await;
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{url}");
        }
    }

    // Whole tokens only: a longer word is not blocked.
    let r = send(&app.router, get("/search?q=zzforbiddenzzz")).await;
    assert!(!r.body.contains("This search is blocked"));
    assert_eq!(metrics().counter("dc3_blocked_queries_total"), blocked + 5);
}

#[tokio::test]
async fn short_prefixes_of_blocked_terms_are_blocked() {
    let _serial = serial().await;
    // Clean name, blocked term only in the files: reachable only through
    // prefix expansion.
    let app = app(vec![torrent(1, "holiday photos", &[("zzforbiddenzz/a.jpg", 1)])]);
    let blocked = metrics().counter("dc3_blocked_queries_total");
    let searches = metrics().histogram_count("dc3_search_seconds");
    for q in ["zz", "zzforbiddenz"] {
        let r = send(&app.router, get(&format!("/search?q={}", enc(q)))).await;
        assert_eq!(r.status, StatusCode::OK, "{q}");
        assert!(
            r.body
                .contains("Searches for child sexual abuse material are blocked."),
            "{q}"
        );
        assert!(!r.body.contains("result-title"), "{q}");
        // The blocked query is searched never and shown nowhere.
        let r = send(
            &app.router,
            get(&format!("/api/v1/search?q={}", enc(q))),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{q}");
        let json = r.json();
        assert_eq!(json["blocked"], true, "{q}");
        assert_eq!(json["query"], "", "{q}");
    }
    assert_eq!(metrics().counter("dc3_blocked_queries_total"), blocked + 4);
    assert_eq!(app.backend.get_many_calls(), 0);
    assert_eq!(metrics().histogram_count("dc3_search_seconds"), searches);
}

#[tokio::test]
async fn over_long_queries_are_refused_before_the_policy_check() {
    let _serial = serial().await;
    let app = app(vec![torrent(1, "zzforbiddenzz movie", &[("a", 1)])]);
    let blocked = metrics().counter("dc3_blocked_queries_total");
    // Over the length limit, with a blocked term: refused as too long, so
    // the term matcher never sees more than the limit.
    let q = format!("zzforbiddenzz {}", "\u{FDFA}".repeat(250));
    let r = send(&app.router, get(&format!("/search?q={}", enc(&q)))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.body.contains("Your search is too long."));
    // The refused query is not echoed back.
    assert!(!r.body.to_lowercase().contains("zzf"));
    let r = send(&app.router, get(&format!("/api/v1/search?q={}", enc(&q)))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(!r.body.to_lowercase().contains("zzf"));
    assert_eq!(metrics().counter("dc3_blocked_queries_total"), blocked);
    assert_eq!(app.backend.get_many_calls(), 0);
}

#[tokio::test]
async fn blocked_terms_in_stored_text_hide_torrents() {
    let _serial = serial().await;
    let app = app(vec![
        torrent(1, "holiday photos", &[("zzforbiddenzz/a.jpg", 1)]),
        torrent(2, "holiday video", &[("b.mp4", 1)]),
        torrent(3, "holiday zzforbiddenzz", &[("c.mp4", 1)]),
    ]);
    // Result lists check names (the indexer checked the paths).
    let r = send(&app.router, get("/search?q=holiday")).await;
    let mut names = result_names(&r.body);
    names.sort();
    assert_eq!(names, ["holiday photos", "holiday video"]);
    let r = send(&app.router, get("/api/v1/search?q=holiday")).await;
    assert_eq!(r.json()["results"].as_array().unwrap().len(), 2);
    // Detail pages check the name and every listed path.
    for id in [1, 3] {
        let key = key_for(id).to_hex();
        let r = send(&app.router, get(&format!("/t/{key}"))).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{id}");
        let r = send(&app.router, get(&format!("/api/v1/torrents/{key}"))).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{id}");
    }
    let r = send(&app.router, get(&format!("/t/{}", key_for(2).to_hex()))).await;
    assert_eq!(r.status, StatusCode::OK);
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
