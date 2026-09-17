//! Least-privilege check against a database initialised by
//! `deploy/postgres/init/10-roles.sh`. Run by
//! `crates/dc3-store/scripts/verify-roles.sh`; skipped unless the four
//! `DC3_ROLES_*_URL` variables are set.
// Helpers outside `#[test]` functions are test code too.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use dc3_core::{AnyKey, DhtKey};
use dc3_store::{
    DenyReason, FileRow, NewReport, NewTorrent, Observation, ReportAction, ReportReason,
    SETTING_AUTOHIDE_PER_HOUR, Store, StoreError,
};

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

fn report(key: DhtKey, reason: ReportReason) -> NewReport {
    NewReport {
        key: AnyKey::V1OrDht(key),
        reason,
        message: "please review".into(),
        contact: None,
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

    // submit_report belongs to the migration role and runs with its rights.
    let (fn_owner, secdef): (String, bool) = sqlx::query_as(
        "SELECT pg_get_userbyid(proowner)::text, prosecdef FROM pg_proc \
         WHERE oid = 'submit_report(bytea, text, text, text)'::regprocedure",
    )
    .fetch_one(owner.pool())
    .await
    .unwrap();
    assert_eq!(fn_owner, "dc3_owner");
    assert!(secdef);

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
    assert!(!crawler.is_denied(&[k.as_bytes()]).await.unwrap());
    crawler
        .deny(bad.as_bytes(), DenyReason::CsamAuto, None, "crawler")
        .await
        .unwrap();
    crawler.purge_gave_up(Duration::from_secs(1)).await.unwrap();
    crawler.observe(&[obs(k, 1, false)], 1_000).await.unwrap();
    let live = crawler.scan_live(0, 100).await.unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(
        (live[0].id, live[0].paths.clone()),
        (id, vec!["f".to_owned()])
    );
    must_deny(&crawler, "DELETE FROM torrents").await;
    // The moderation columns are out of the crawler's reach.
    must_deny(&crawler, "UPDATE torrents SET hidden_at = NULL").await;
    must_deny(&crawler, "UPDATE torrents SET hidden_at = now()").await;
    must_deny(&crawler, "UPDATE torrents SET reviewed_at = now()").await;
    must_deny(
        &crawler,
        "INSERT INTO torrents (dht_key, name, total_size, file_count, change_seq, reviewed_at) \
         VALUES (decode(repeat('01', 20), 'hex'), 'x', 0, 0, 0, now())",
    )
    .await;
    must_deny(
        &crawler,
        "INSERT INTO torrents (dht_key, name, total_size, file_count, change_seq, hidden_at) \
         VALUES (decode(repeat('01', 20), 'hex'), 'x', 0, 0, 0, now())",
    )
    .await;
    must_allow(
        &crawler,
        "UPDATE torrents SET seen_count = seen_count WHERE false",
    )
    .await;
    must_deny(&crawler, "DELETE FROM denylist").await;
    must_deny(&crawler, "SELECT * FROM reports").await;
    must_deny(&crawler, "SELECT * FROM audit_log").await;
    must_deny(&crawler, "SELECT * FROM settings").await;
    must_deny(&crawler, "CREATE TABLE evil (x int)").await;
    must_deny(
        &crawler,
        "SELECT * FROM submit_report(decode(repeat('00', 20), 'hex'), 'other', '', NULL)",
    )
    .await;
    assert_denied("crawler stats", crawler.stats().await);

    // --- indexer: read-only, torrents and denylist only.
    let mark = indexer.high_water_mark().await.unwrap().unwrap();
    assert!(mark > 0);
    let rows = indexer.changes_since(0, mark, 100).await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.id == id && r.visible && r.files_text == "f")
    );
    must_deny(&indexer, "UPDATE torrents SET name = 'x'").await;
    must_deny(&indexer, "INSERT INTO denylist (key, reason, created_by) VALUES (decode(repeat('00', 20), 'hex'), 'other', 'x')").await;
    must_deny(&indexer, "SELECT nextval('change_seq')").await;
    must_deny(&indexer, "SELECT * FROM reports").await;
    must_deny(&indexer, "SELECT count(*) FROM pending").await;
    must_deny(&indexer, "SELECT * FROM stats_daily").await;
    must_deny(&indexer, "SELECT * FROM settings").await;
    must_deny(
        &indexer,
        "SELECT * FROM submit_report(decode(repeat('00', 20), 'hex'), 'other', '', NULL)",
    )
    .await;
    must_allow(&indexer, "SELECT count(*) FROM denylist").await;
    assert_denied("indexer pending_depth", indexer.pending_depth().await);

    // --- web: reads, and reports through submit_report only.
    assert!(web.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_some());
    assert_eq!(web.get_many(&[id]).await.unwrap().len(), 1);
    let stats = web.public_stats().await.unwrap();
    assert_eq!((stats.torrents, stats.added_today), (1, 1));
    web.daily_stats(7).await.unwrap();
    assert_denied("web stats", web.stats().await);

    let hid = web
        .submit_report(&report(k, ReportReason::Csam))
        .await
        .unwrap();
    assert!(hid.hidden);
    assert!(web.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_none());
    let other = web
        .submit_report(&report(DhtKey([10; 20]), ReportReason::Copyright))
        .await
        .unwrap();
    assert!(!other.hidden);
    // The function still validates direct calls.
    let err = sqlx::query("SELECT * FROM submit_report('\\x00'::bytea, 'csam', '', NULL)")
        .execute(web.pool())
        .await
        .unwrap_err();
    assert_eq!(sqlx_state(&err).as_deref(), Some("22023"));

    must_deny(&web, "INSERT INTO reports (dht_key, reason, message) VALUES (decode(repeat('00', 20), 'hex'), 'other', '')").await;
    must_deny(&web, "UPDATE torrents SET hidden_at = now()").await;
    must_deny(&web, "UPDATE torrents SET hidden_at = NULL").await;
    must_deny(&web, "UPDATE torrents SET name = 'x'").await;
    must_deny(&web, "UPDATE torrents SET deleted_at = now()").await;
    must_deny(&web, "DELETE FROM torrents").await;
    must_deny(&web, "SELECT id FROM reports").await;
    must_deny(&web, "SELECT message FROM reports").await;
    must_deny(&web, "SELECT contact FROM reports").await;
    must_deny(&web, "DELETE FROM reports").await;
    must_deny(&web, "UPDATE reports SET status = 'dismissed'").await;
    must_deny(&web, "SELECT gave_up FROM pending").await;
    must_deny(&web, "SELECT count(*) FROM pending").await;
    must_deny(&web, "SELECT nextval('change_seq')").await;
    must_deny(&web, "INSERT INTO denylist (key, reason, created_by) VALUES (decode(repeat('00', 20), 'hex'), 'other', 'x')").await;
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
        "UPDATE settings SET value = '100000' WHERE key = 'autohide_per_hour'",
    )
    .await;
    must_deny(
        &web,
        "CREATE FUNCTION evil() RETURNS int LANGUAGE sql RETURN 1",
    )
    .await;
    must_deny(
        &web,
        "ALTER FUNCTION submit_report(bytea, text, text, text) SECURITY INVOKER",
    )
    .await;
    must_deny(&web, "CREATE TEMP TABLE scratch (x int)").await;
    assert_denied("web list_reports", web.list_reports(None, 10).await);
    assert_denied(
        "web deny",
        web.deny(k.as_bytes(), DenyReason::Other, None, "web").await,
    );
    assert_denied(
        "web resolve_report",
        web.resolve_report(hid.report_id, ReportAction::Dismiss, None, "web")
            .await,
    );
    assert_denied(
        "web set_setting",
        web.set_setting(SETTING_AUTOHIDE_PER_HOUR, "1000").await,
    );
    assert_denied("web pending_depth", web.pending_depth().await);

    // --- owner: admin flows.
    assert_eq!(owner.list_reports(None, 10).await.unwrap().len(), 2);
    owner
        .resolve_report(hid.report_id, ReportAction::Dismiss, None, "admin")
        .await
        .unwrap();
    let back = web.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().unwrap();
    assert!(back.reviewed_at.is_some());
    // Reviewed: a new CSAM report does not hide it again.
    assert!(
        !web.submit_report(&report(k, ReportReason::Csam))
            .await
            .unwrap()
            .hidden
    );
    assert!(
        owner
            .set_setting(SETTING_AUTOHIDE_PER_HOUR, "10")
            .await
            .unwrap()
    );
    assert_eq!(
        owner
            .get_setting(SETTING_AUTOHIDE_PER_HOUR)
            .await
            .unwrap()
            .as_deref(),
        Some("10")
    );
    assert_eq!(owner.stats().await.unwrap().open_reports, 2);
    assert!(owner.undeny(bad.as_bytes(), "admin").await.unwrap());
}
