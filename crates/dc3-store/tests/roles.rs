//! Least-privilege check against a database initialised by
//! `deploy/postgres/init/10-roles.sh`. Run by
//! `crates/dc3-store/scripts/verify-roles.sh`; skipped unless the four
//! `DC3_ROLES_*_URL` variables are set.
// Helpers outside `#[test]` functions are test code too.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use dc3_core::{AnyKey, DhtKey};
use dc3_store::{FileRow, NewTorrent, Observation, Store, StoreError};

struct Urls {
    owner: String,
    crawler: String,
    indexer: String,
    web: String,
}

fn urls() -> Option<Urls> {
    let get = |v: &str| std::env::var(v).ok().filter(|s| !s.is_empty());
    Some(Urls {
        owner: get("DC3_ROLES_OWNER_URL")?,
        crawler: get("DC3_ROLES_CRAWLER_URL")?,
        indexer: get("DC3_ROLES_INDEXER_URL")?,
        web: get("DC3_ROLES_WEB_URL")?,
    })
}

fn sqlstate(e: &StoreError) -> Option<String> {
    match e {
        StoreError::Database(e) => sqlx_state(e),
        _ => None,
    }
}

fn sqlx_state(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
}

const DENIED: &str = "42501"; // insufficient_privilege

async fn must_deny(store: &Store, sql: &'static str) {
    let err = sqlx::query(sql).execute(store.pool()).await.unwrap_err();
    assert_eq!(sqlx_state(&err).as_deref(), Some(DENIED), "{sql}: {err}");
}

async fn must_allow(store: &Store, sql: &'static str) {
    sqlx::query(sql)
        .execute(store.pool())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn assert_denied<T: std::fmt::Debug>(what: &str, r: Result<T, StoreError>) {
    match r {
        Err(e) => assert_eq!(sqlstate(&e).as_deref(), Some(DENIED), "{what}: {e}"),
        Ok(v) => panic!("{what} should be denied, got {v:?}"),
    }
}

fn obs(key: DhtKey, sightings: u32, priority: bool) -> Observation {
    Observation {
        key,
        sightings,
        priority,
    }
}

async fn idle_timeout(store: &Store) -> String {
    sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
        .fetch_one(store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn roles_have_least_privilege() {
    let Some(urls) = urls() else {
        eprintln!("DC3_ROLES_*_URL not set; skipping");
        return;
    };
    let owner = Store::connect(&urls.owner, 2).await.unwrap();
    owner.migrate().await.unwrap();
    owner.ping().await.unwrap();

    // lz4 applied where the server supports it (the official image does).
    let compression: Option<String> = sqlx::query_scalar(
        "SELECT attcompression::text FROM pg_attribute \
         WHERE attrelid = 'torrents'::regclass AND attname = 'files'",
    )
    .fetch_one(owner.pool())
    .await
    .unwrap();
    assert_eq!(compression.as_deref(), Some("l"));

    // No report objects: no reports table, no submit_report function.
    let regclass: Option<String> = sqlx::query_scalar("SELECT to_regclass('public.reports')::text")
        .fetch_one(owner.pool())
        .await
        .unwrap();
    assert_eq!(regclass, None);
    let fn_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_proc WHERE proname = 'submit_report'")
            .fetch_one(owner.pool())
            .await
            .unwrap();
    assert_eq!(fn_count, 0);

    let crawler = Store::connect(&urls.crawler, 2).await.unwrap();
    let indexer = Store::connect(&urls.indexer, 2).await.unwrap();
    let web = Store::connect(&urls.web, 2).await.unwrap();
    for store in [&owner, &crawler, &indexer, &web] {
        assert_eq!(idle_timeout(store).await, "1min");
    }

    // --- crawler: the whole pipeline works.
    let k = DhtKey([7; 20]);
    let bad = DhtKey([8; 20]);
    let o = crawler
        .observe(&[obs(k, 3, false), obs(bad, 1, true)], 1_000)
        .await
        .unwrap();
    assert_eq!(o.queued, 2);
    assert_eq!(crawler.pending_depth().await.unwrap(), 2);
    let o = crawler
        .observe(&[obs(DhtKey([9; 20]), 1, false)], 2)
        .await
        .unwrap();
    assert_eq!((o.queued, o.dropped), (0, 1));
    let claimed = crawler.claim(10, Duration::from_secs(60)).await.unwrap();
    assert_eq!(claimed.len(), 2);
    assert!(crawler.renew(&k, Duration::from_secs(120)).await.unwrap());
    let t = NewTorrent {
        dht_key: k,
        info_hash_v1: Some(k),
        info_hash_v2: None,
        name: "roles".into(),
        total_size: 1,
        file_count: 1,
        files: vec![FileRow {
            path: "f".into(),
            size: 1,
        }],
        files_truncated: false,
        piece_length: Some(16384),
    };
    let id = crawler.complete(&k, &t).await.unwrap();
    crawler.fail(&bad).await.unwrap();
    assert!(crawler.give_up(&bad).await.unwrap());
    crawler.purge_gave_up(Duration::from_secs(1)).await.unwrap();
    crawler.observe(&[obs(k, 1, false)], 1_000).await.unwrap();
    must_deny(&crawler, "DELETE FROM torrents").await;
    must_allow(
        &crawler,
        "UPDATE torrents SET seen_count = seen_count WHERE false",
    )
    .await;
    must_deny(&crawler, "SELECT * FROM audit_log").await;
    must_deny(&crawler, "SELECT * FROM settings").await;
    must_deny(&crawler, "CREATE TABLE evil (x int)").await;
    // stats() reads only pending/torrents, which the crawler can
    // already SELECT individually for the pipeline.
    let stats = crawler.stats().await.unwrap();
    assert_eq!(stats.torrents, 1);

    // --- indexer: read-only, torrents only.
    let mark = indexer.high_water_mark().await.unwrap().unwrap();
    assert!(mark > 0);
    let rows = indexer.changes_since(0, mark, 100).await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.id == id && r.visible && r.files_text == "f")
    );
    must_deny(&indexer, "UPDATE torrents SET name = 'x'").await;
    must_deny(&indexer, "SELECT nextval('change_seq')").await;
    must_deny(&indexer, "SELECT count(*) FROM pending").await;
    must_deny(&indexer, "SELECT * FROM stats_daily").await;
    must_deny(&indexer, "SELECT * FROM settings").await;
    assert_denied("indexer pending_depth", indexer.pending_depth().await);

    // --- web: reads only.
    assert!(web.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_some());
    assert_eq!(web.get_many(&[id]).await.unwrap().len(), 1);
    let stats = web.public_stats().await.unwrap();
    assert_eq!((stats.torrents, stats.added_today), (1, 1));
    web.daily_stats(7).await.unwrap();
    assert_denied("web stats", web.stats().await);

    must_deny(&web, "UPDATE torrents SET name = 'x'").await;
    must_deny(&web, "UPDATE torrents SET deleted_at = now()").await;
    must_deny(&web, "DELETE FROM torrents").await;
    must_deny(&web, "SELECT gave_up FROM pending").await;
    must_deny(&web, "SELECT count(*) FROM pending").await;
    must_deny(&web, "SELECT nextval('change_seq')").await;
    must_deny(
        &web,
        "INSERT INTO audit_log (actor, action, subject) VALUES ('a', 'b', 'c')",
    )
    .await;
    must_deny(&web, "SELECT * FROM audit_log").await;
    must_deny(
        &web,
        "INSERT INTO stats_daily (day) VALUES (current_date - 100)",
    )
    .await;
    must_deny(&web, "SELECT * FROM settings").await;
    must_deny(
        &web,
        "UPDATE settings SET value = 'hello' WHERE key = 'banner'",
    )
    .await;
    must_deny(
        &web,
        "CREATE FUNCTION evil() RETURNS int LANGUAGE sql RETURN 1",
    )
    .await;
    must_deny(&web, "CREATE TEMP TABLE scratch (x int)").await;
    assert_denied("web set_setting", web.set_setting("banner", "hello").await);
    assert_denied("web pending_depth", web.pending_depth().await);

    // --- owner: admin flows (settings, stats).
    assert!(owner.set_setting("banner", "hello").await.unwrap());
    assert_eq!(
        owner.get_setting("banner").await.unwrap().as_deref(),
        Some("hello")
    );
    assert_eq!(owner.stats().await.unwrap().torrents, 1);
}
