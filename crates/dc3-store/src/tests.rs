//! Database tests. Each `#[sqlx::test]` gets a fresh database with the
//! migrations applied; `DATABASE_URL` must name a superuser connection.
#![allow(clippy::arithmetic_side_effects)]

use std::collections::HashSet;
use std::time::{Duration, Instant};

use dc3_core::{AnyKey, DhtKey, InfoHashV2};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use crate::web::submit_error;
use crate::*;

/// A `max_pending` that never gates.
const NO_LIMIT: i64 = i64::MAX;

fn key(n: u8) -> DhtKey {
    let mut b = [0u8; 20];
    b[0] = n;
    b[19] = 0xa5;
    DhtKey(b)
}

fn v2(n: u8) -> InfoHashV2 {
    let mut b = [0u8; 32];
    b[0] = n;
    b[1] = 0x77;
    b[31] = 0x5a;
    InfoHashV2(b)
}

fn torrent(k: DhtKey, name: &str) -> NewTorrent {
    NewTorrent {
        dht_key: k,
        info_hash_v1: Some(k),
        info_hash_v2: None,
        name: name.to_owned(),
        total_size: 3000,
        file_count: 2,
        files: vec![
            FileRow {
                path: "a/one.txt".into(),
                size: 1000,
            },
            FileRow {
                path: "b/two.bin".into(),
                size: 2000,
            },
        ],
        files_truncated: false,
        piece_length: Some(16384),
    }
}

/// A v2-only torrent, keyed by its truncated hash.
fn torrent_v2(h: InfoHashV2, name: &str) -> NewTorrent {
    NewTorrent {
        dht_key: h.truncated(),
        info_hash_v1: None,
        info_hash_v2: Some(h),
        ..torrent(h.truncated(), name)
    }
}

/// A torrent with `n` files named `dir/fNNN`.
fn torrent_with_files(k: DhtKey, n: usize) -> NewTorrent {
    let files: Vec<FileRow> = (0..n)
        .map(|i| FileRow {
            path: format!("dir/f{i:03}"),
            size: 1,
        })
        .collect();
    NewTorrent {
        total_size: n as u64,
        file_count: n as u64,
        files,
        ..torrent(k, "many files")
    }
}

fn obs(k: DhtKey, n: u32) -> Observation {
    Observation {
        key: k,
        sightings: n,
        priority: false,
    }
}

fn prio(k: DhtKey, n: u32) -> Observation {
    Observation {
        priority: true,
        ..obs(k, n)
    }
}

fn report(key: AnyKey, reason: ReportReason, message: &str) -> NewReport {
    NewReport {
        key,
        reason,
        message: message.to_owned(),
        contact: None,
    }
}

fn csam(k: DhtKey) -> NewReport {
    report(AnyKey::V1OrDht(k), ReportReason::Csam, "")
}

fn code_of(e: &sqlx::Error) -> Option<String> {
    sqlstate(e).map(|c| c.into_owned())
}

async fn change_seq_of(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar("SELECT change_seq FROM torrents WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn raw_torrent(pool: &PgPool, id: i64) -> TorrentRecord {
    let row = sqlx::query(concat!(
        "SELECT ",
        crate::types::torrent_columns_base!(),
        ", t.files FROM torrents t WHERE t.id = $1"
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    TorrentRecord::from_row(&row).unwrap()
}

async fn today(pool: &PgPool) -> DailyStats {
    let s = Store::from_pool(pool.clone());
    s.daily_stats(1).await.unwrap().pop().unwrap_or(DailyStats {
        day: chrono::NaiveDate::MIN,
        discovered: 0,
        fetched: 0,
        fetch_failed: 0,
        blocked: 0,
    })
}

async fn audit_count(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1")
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn report_torrent(pool: &PgPool, report_id: i64) -> Option<i64> {
    sqlx::query_scalar("SELECT torrent_id FROM reports WHERE id = $1")
        .bind(report_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn pending_seen(pool: &PgPool, k: DhtKey) -> Option<i64> {
    sqlx::query_scalar("SELECT seen_count FROM pending WHERE dht_key = $1")
        .bind(k.0.as_slice())
        .fetch_optional(pool)
        .await
        .unwrap()
}

/// Seconds until the lease on `k` expires.
async fn lease_left(pool: &PgPool, k: DhtKey) -> f64 {
    sqlx::query_scalar(
        "SELECT extract(epoch FROM lease_until - now())::float8 FROM pending WHERE dht_key = $1",
    )
    .bind(k.0.as_slice())
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A second pool on the test database with the store's session settings.
async fn session_pool(pool: &PgPool, max_connections: u32) -> PgPool {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(session_options((*pool.connect_options()).clone()))
        .await
        .unwrap()
}

/// Inserts `n` bare torrents directly, for paging tests.
async fn insert_bare_torrents(pool: &PgPool, n: i64) {
    sqlx::query(
        "INSERT INTO torrents (dht_key, name, total_size, file_count, change_seq) \
         SELECT decode(lpad(to_hex(i), 40, '0'), 'hex'), 'bare ' || i, 0, 0, nextval('change_seq') \
           FROM generate_series(1000000, 1000000 + $1 - 1) AS i",
    )
    .bind(n)
    .execute(pool)
    .await
    .unwrap();
}

#[test]
fn enums_round_trip() {
    for r in DenyReason::ALL {
        assert_eq!(r.as_str().parse::<DenyReason>().unwrap(), *r);
    }
    for r in ReportReason::ALL {
        assert_eq!(r.as_str().parse::<ReportReason>().unwrap(), *r);
    }
    for r in ReportStatus::ALL {
        assert_eq!(r.as_str().parse::<ReportStatus>().unwrap(), *r);
    }
    assert_eq!(DenyReason::CsamAuto.as_str(), "csam-auto");
    assert!("nope".parse::<DenyReason>().is_err());
    assert_eq!(key_lock_id(&[1, 2, 3, 4, 5]), 0x0102_0304);
    assert_eq!(key_lock_id(&[1]), 0x0100_0000);
    assert_eq!(hex_of(&[0, 0xab]), "00ab");
}

#[test]
fn helpers_and_constants() {
    // The database function spells these in decimal.
    assert_eq!(CHANGE_LOCK_KEY, 7_233_681_928_533_508_097);
    assert_eq!(AUTOHIDE_LOCK_KEY, 7_233_681_950_024_925_185);
    assert_ne!(CHANGE_LOCK_KEY, AUTOHIDE_LOCK_KEY);

    for ok in ["0", "7", "60", "999999999"] {
        assert!(is_numeric_setting_value(ok), "{ok}");
    }
    for bad in [
        "",
        "-1",
        "+1",
        " 1",
        "1 ",
        "1.0",
        "1e3",
        "abc",
        "1234567890",
        "٣",
    ] {
        assert!(!is_numeric_setting_value(bad), "{bad}");
    }
    assert_eq!(
        MAX_NUMERIC_SETTING.to_string().len(),
        MAX_NUMERIC_SETTING_DIGITS
    );
    assert!(check_key_len(&[0; 20]).is_ok());
    assert!(check_key_len(&[0; 32]).is_ok());
    assert!(matches!(
        check_key_len(&[0; 21]),
        Err(StoreError::Invalid(_))
    ));

    let opts = session_options(sqlx::postgres::PgConnectOptions::new());
    assert!(
        format!("{opts:?}").contains("idle_in_transaction_session_timeout=60s"),
        "{opts:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn migrations_and_ping(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    s.ping().await.unwrap();
    // Running again is a no-op.
    s.migrate().await.unwrap();
    assert_eq!(s.high_water_mark().await.unwrap(), Some(0));
    assert_eq!(s.stats().await.unwrap(), StoreStats::default());
    assert_eq!(s.public_stats().await.unwrap(), PublicStats::default());
    assert_eq!(s.pending_depth().await.unwrap(), 0);

    // Seeded settings match the crate's defaults.
    assert_eq!(
        s.get_setting(SETTING_AUTOHIDE_PER_HOUR).await.unwrap(),
        Some(DEFAULT_AUTOHIDE_PER_HOUR.to_string())
    );
    assert_eq!(
        s.get_setting(SETTING_OPEN_REPORTS_CAP).await.unwrap(),
        Some(DEFAULT_OPEN_REPORTS_CAP.to_string())
    );

    // submit_report is SECURITY DEFINER with a pinned search_path, uses the
    // crate's lock keys, and PUBLIC cannot call it.
    let row = sqlx::query(
        "SELECT prosecdef, proconfig, prosrc, \
                has_function_privilege('public', p.oid, 'EXECUTE') AS public_exec \
           FROM pg_proc p WHERE p.oid = 'submit_report(bytea, text, text, text)'::regprocedure",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(row.get::<bool, _>("prosecdef"));
    assert_eq!(
        row.get::<Vec<String>, _>("proconfig"),
        vec!["search_path=pg_catalog, public, pg_temp".to_owned()]
    );
    let src: String = row.get("prosrc");
    assert!(src.contains(&CHANGE_LOCK_KEY.to_string()));
    assert!(src.contains(&AUTOHIDE_LOCK_KEY.to_string()));
    assert!(src.contains(&format!(
        "c_default_cap      CONSTANT bigint := {DEFAULT_OPEN_REPORTS_CAP};"
    )));
    assert!(src.contains(&format!(
        "c_default_budget   CONSTANT bigint := {DEFAULT_AUTOHIDE_PER_HOUR};"
    )));
    assert!(src.contains(&format!(
        "c_max_message      CONSTANT integer := {REPORT_MESSAGE_MAX_CHARS};"
    )));
    assert!(src.contains(&format!(
        "c_max_contact      CONSTANT integer := {REPORT_CONTACT_MAX_CHARS};"
    )));
    assert!(!row.get::<bool, _>("public_exec"));
    let public_exec: bool = sqlx::query_scalar(
        "SELECT has_function_privilege('public', 'dc3_key_denied(bytea, bytea, bytea)', 'EXECUTE')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!public_exec);
}

#[sqlx::test(migrations = "./migrations")]
async fn connections_get_session_settings(pool: PgPool) {
    let s = Store::connect_with((*pool.connect_options()).clone(), 1)
        .await
        .unwrap();
    let timeout: String = sqlx::query_scalar("SHOW idle_in_transaction_session_timeout")
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(timeout, "1min");
    s.ping().await.unwrap();
    s.pool().close().await;

    assert!(matches!(
        Store::connect("not a url", 1).await,
        Err(StoreError::Database(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn observe_unknown_known_and_denied(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let (a, b, c) = (key(1), key(2), key(3));

    // Unknown keys are queued; duplicates merge.
    let o = s
        .observe(&[obs(a, 1), obs(b, 2), obs(a, 3), obs(c, 0)], NO_LIMIT)
        .await
        .unwrap();
    assert_eq!(
        o,
        ObserveOutcome {
            known: 0,
            queued: 2,
            denied: 0,
            dropped: 0
        }
    );
    assert_eq!(pending_seen(&pool, a).await, Some(4));
    assert_eq!(pending_seen(&pool, c).await, None);
    assert_eq!(today(&pool).await.discovered, 2);

    // Already pending: counted, not queued again.
    let o = s.observe(&[obs(a, 1)], NO_LIMIT).await.unwrap();
    assert_eq!(o, ObserveOutcome::default());
    assert_eq!(pending_seen(&pool, a).await, Some(5));
    assert_eq!(today(&pool).await.discovered, 2);

    // Denied keys are not queued, including by a v2 hash whose prefix is the key.
    let h = v2(9);
    s.deny(h.as_bytes(), DenyReason::Dmca, None, "test")
        .await
        .unwrap();
    s.deny(c.as_bytes(), DenyReason::Abuse, Some("n"), "test")
        .await
        .unwrap();
    let o = s
        .observe(&[obs(c, 1), prio(h.truncated(), 1)], NO_LIMIT)
        .await
        .unwrap();
    assert_eq!(
        o,
        ObserveOutcome {
            known: 0,
            queued: 0,
            denied: 2,
            dropped: 0
        }
    );

    // Known keys: counted, not queued.
    let id = s.complete(&b, &torrent(b, "bee")).await.unwrap();
    let o = s.observe(&[obs(b, 1)], NO_LIMIT).await.unwrap();
    assert_eq!(
        o,
        ObserveOutcome {
            known: 1,
            ..ObserveOutcome::default()
        }
    );
    let r = raw_torrent(&pool, id).await;
    assert_eq!(r.seen_count, 3); // 2 from pending + 1
    assert_eq!(pending_seen(&pool, b).await, None);

    // A hybrid stored under its v1 key is known by its truncated v2 key too.
    let hy = v2(20);
    let k1 = key(21);
    let t = NewTorrent {
        info_hash_v2: Some(hy),
        ..torrent(k1, "hybrid")
    };
    let hid = s.complete(&k1, &t).await.unwrap();
    let o = s
        .observe(&[obs(hy.truncated(), 5)], NO_LIMIT)
        .await
        .unwrap();
    assert_eq!(o.known, 1);
    assert_eq!(raw_torrent(&pool, hid).await.seen_count, 6);

    assert_eq!(
        s.observe(&[], NO_LIMIT).await.unwrap(),
        ObserveOutcome::default()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn observe_gates_new_keys_on_max_pending(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let known = key(1);
    let denied = key(8);
    s.complete(&known, &torrent(known, "known")).await.unwrap();
    s.deny(denied.as_bytes(), DenyReason::Other, None, "test")
        .await
        .unwrap();
    s.observe(&[obs(key(2), 1), obs(key(3), 1)], NO_LIMIT)
        .await
        .unwrap();
    assert_eq!(s.pending_depth().await.unwrap(), 2);

    // At the limit: known keys and queued keys are still counted, ordinary new
    // keys are dropped, priority keys are queued. A key with one priority
    // entry is a priority key.
    let o = s
        .observe(
            &[
                obs(known, 1),
                obs(key(2), 5),
                obs(key(4), 1),
                obs(key(5), 1),
                prio(key(5), 1),
                obs(denied, 1),
            ],
            2,
        )
        .await
        .unwrap();
    assert_eq!(
        o,
        ObserveOutcome {
            known: 1,
            queued: 1,
            denied: 1,
            dropped: 1
        }
    );
    assert_eq!(pending_seen(&pool, key(2)).await, Some(6));
    assert_eq!(pending_seen(&pool, key(4)).await, None);
    assert_eq!(pending_seen(&pool, key(5)).await, Some(2));
    let r = s
        .get_by_key(&AnyKey::V1OrDht(known))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.seen_count, 2);

    // Above the limit (depth 3, limit 2), the same holds; priority keys
    // already queued are counted like any other.
    let o = s
        .observe(&[obs(key(4), 2), prio(key(3), 1), prio(key(6), 1)], 2)
        .await
        .unwrap();
    assert_eq!(
        o,
        ObserveOutcome {
            known: 0,
            queued: 1,
            denied: 0,
            dropped: 1
        }
    );
    assert_eq!(pending_seen(&pool, key(3)).await, Some(2));

    // Below the limit, ordinary keys are queued again.
    let o = s.observe(&[obs(key(4), 1)], 100).await.unwrap();
    assert_eq!((o.queued, o.dropped), (1, 0));

    // Between the limit and twice the limit (depth 5, limit 3), only
    // priority keys are admitted.
    let o = s
        .observe(&[obs(key(7), 1), prio(key(9), 1)], 3)
        .await
        .unwrap();
    assert_eq!((o.queued, o.dropped), (1, 1));
    assert_eq!(pending_seen(&pool, key(7)).await, None);

    // key(2), key(3), key(5), key(6), key(4), key(9).
    assert_eq!(today(&pool).await.discovered, 6);
    assert_eq!(s.pending_depth().await.unwrap(), 6);
}

#[sqlx::test(migrations = "./migrations")]
async fn popularity_bumps_change_seq_only_on_log2_change(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    let id = s.complete(&k, &torrent(k, "pop")).await.unwrap(); // seen_count = 1
    let mut last = change_seq_of(&pool, id).await;
    // seen_count after each observation, and whether the log2 bucket changed.
    let steps: [(u32, u64, bool); 7] = [
        (1, 2, true),  // 1 -> 2
        (1, 3, false), // 2 -> 3
        (1, 4, true),  // 3 -> 4
        (3, 7, false), // 4 -> 7
        (1, 8, true),  // 7 -> 8
        (100, 108, true),
        (10, 118, false),
    ];
    for (n, expect_seen, bumped) in steps {
        s.observe(&[obs(k, n)], NO_LIMIT).await.unwrap();
        let r = raw_torrent(&pool, id).await;
        assert_eq!(r.seen_count, expect_seen);
        let now = r.change_seq;
        assert_eq!(now != last, bumped, "at seen_count {expect_seen}");
        if bumped {
            assert!(now > last);
        }
        last = now;
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn claim_leases_without_overlap(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let keys: Vec<Observation> = (0..100u8).map(|i| obs(key(i), 1)).collect();
    s.observe(&keys, NO_LIMIT).await.unwrap();

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let mut got = Vec::new();
            loop {
                let batch = s.claim(7, Duration::from_secs(3600)).await.unwrap();
                if batch.is_empty() {
                    break;
                }
                got.extend(batch.into_iter().map(|p| p.dht_key));
            }
            got
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        all.extend(t.await.unwrap());
    }
    assert_eq!(all.len(), 100, "every item claimed exactly once");
    let distinct: HashSet<_> = all.iter().collect();
    assert_eq!(distinct.len(), 100);

    // Everything is leased now.
    assert!(
        s.claim(10, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        s.claim(0, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );

    // An expired lease makes the item claimable again.
    sqlx::query("UPDATE pending SET lease_until = now() - interval '1 second' WHERE dht_key = $1")
        .bind(key(5).0.as_slice())
        .execute(&pool)
        .await
        .unwrap();
    let again = s.claim(10, Duration::from_secs(60)).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].dht_key, key(5));
    assert_eq!(again[0].attempts, 0);
    assert_eq!(again[0].seen_count, 1);
    let lease = lease_left(&pool, key(5)).await;
    assert!(lease > 50.0 && lease <= 60.0, "lease {lease}");
}

#[sqlx::test(migrations = "./migrations")]
async fn renew_extends_only_live_leases(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    let hour = Duration::from_secs(3600);
    s.observe(&[obs(k, 1)], NO_LIMIT).await.unwrap();

    assert!(!s.renew(&k, hour).await.unwrap(), "never claimed");
    assert!(!s.renew(&key(99), hour).await.unwrap(), "not queued");

    assert_eq!(s.claim(1, Duration::from_secs(60)).await.unwrap().len(), 1);
    assert!(s.renew(&k, hour).await.unwrap());
    let left = lease_left(&pool, k).await;
    assert!(left > 3590.0 && left <= 3600.0, "lease {left}");

    // A shorter renewal keeps the longer lease.
    assert!(s.renew(&k, Duration::from_secs(10)).await.unwrap());
    assert!(lease_left(&pool, k).await > 3590.0);

    // An expired lease is not revived.
    sqlx::query("UPDATE pending SET lease_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!s.renew(&k, hour).await.unwrap());

    // fail() releases the lease.
    assert_eq!(s.claim(1, Duration::from_secs(60)).await.unwrap().len(), 1);
    s.fail(&k).await.unwrap();
    assert!(!s.renew(&k, hour).await.unwrap());

    // A key that gave up is not renewed.
    sqlx::query("UPDATE pending SET gave_up = true, lease_until = now() + interval '1 hour'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!s.renew(&k, hour).await.unwrap());
}

#[sqlx::test(migrations = "./migrations")]
async fn fail_backs_off_and_gives_up(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    s.observe(&[obs(k, 1)], NO_LIMIT).await.unwrap();
    assert!(!s.fail(&key(99)).await.unwrap(), "no queue row");

    for attempt in 1..=6i32 {
        assert_eq!(s.claim(1, Duration::from_secs(60)).await.unwrap().len(), 1);
        let gave_up = s.fail(&k).await.unwrap();
        assert_eq!(gave_up, attempt >= 6);
        let row = sqlx::query(
            "SELECT attempts, gave_up, lease_until, \
             extract(epoch FROM next_attempt_at - last_attempt_at)::float8 AS delay FROM pending WHERE dht_key = $1",
        )
        .bind(k.0.as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<i32, _>("attempts"), attempt);
        assert_eq!(row.get::<bool, _>("gave_up"), attempt >= 6);
        assert!(
            row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("lease_until")
                .is_none()
        );
        let expected = 300.0 * 2f64.powi(attempt - 1);
        assert_eq!(row.get::<f64, _>("delay"), expected);
        // Not due yet; make it due for the next round.
        assert!(
            s.claim(1, Duration::from_secs(60))
                .await
                .unwrap()
                .is_empty()
        );
        sqlx::query("UPDATE pending SET next_attempt_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
    }
    // Given up: never claimed again, but still part of the queue depth.
    assert!(
        s.claim(1, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(today(&pool).await.fetch_failed, 6);
    let st = s.stats().await.unwrap();
    assert_eq!((st.pending, st.gave_up), (0, 1));
    assert_eq!(s.pending_depth().await.unwrap(), 1);

    // The cap applies to large attempt counts.
    sqlx::query("UPDATE pending SET attempts = 40, gave_up = false")
        .execute(&pool)
        .await
        .unwrap();
    s.fail(&k).await.unwrap();
    let delay: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_attempt_at - last_attempt_at)::float8 FROM pending",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(delay, 7.0 * 86400.0);

    // Purge: only rows that gave up long enough ago.
    assert_eq!(s.purge_gave_up(Duration::from_secs(3600)).await.unwrap(), 0);
    sqlx::query("UPDATE pending SET last_attempt_at = now() - interval '2 hours'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(s.purge_gave_up(Duration::from_secs(3600)).await.unwrap(), 1);
    assert_eq!(s.stats().await.unwrap().gave_up, 0);
    assert_eq!(s.pending_depth().await.unwrap(), 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn unstorable_torrent_is_rejected_and_given_up(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    s.observe(&[obs(k, 1)], NO_LIMIT).await.unwrap();
    assert_eq!(s.claim(1, Duration::from_secs(60)).await.unwrap().len(), 1);

    // A verified torrent whose sizes sum past i64::MAX (two files of
    // i64::MAX): refused before the database is touched.
    let big = i64::MAX as u64;
    let t = NewTorrent {
        total_size: big * 2,
        files: vec![
            FileRow {
                path: "a".into(),
                size: big,
            },
            FileRow {
                path: "b".into(),
                size: big,
            },
        ],
        ..torrent(k, "huge")
    };
    assert!(matches!(
        s.complete(&k, &t).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(pending_seen(&pool, k).await, Some(1));
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM torrents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);

    // give_up ends it at once: never claimed again, counted as a failure.
    assert!(!s.give_up(&key(99)).await.unwrap(), "no queue row");
    assert!(s.give_up(&k).await.unwrap());
    let row = sqlx::query("SELECT attempts, gave_up, lease_until FROM pending WHERE dht_key = $1")
        .bind(k.0.as_slice())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<i32, _>("attempts"), 1);
    assert!(row.get::<bool, _>("gave_up"));
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("lease_until")
            .is_none()
    );
    sqlx::query("UPDATE pending SET next_attempt_at = now() - interval '1 day'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        s.claim(10, Duration::from_secs(60))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!s.renew(&k, Duration::from_secs(60)).await.unwrap());
    assert_eq!(today(&pool).await.fetch_failed, 1);
    assert_eq!(s.stats().await.unwrap().gave_up, 1);
    // Purged like any other key that gave up.
    sqlx::query("UPDATE pending SET last_attempt_at = now() - interval '2 hours'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(s.purge_gave_up(Duration::from_secs(3600)).await.unwrap(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn observe_refuses_priority_keys_at_twice_max_pending(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    s.observe(&[obs(key(1), 1), obs(key(2), 1)], NO_LIMIT)
        .await
        .unwrap();
    s.observe(&[obs(key(3), 1), obs(key(4), 1)], NO_LIMIT)
        .await
        .unwrap();
    // Depth 4 = 2 x 2: priority keys are refused too, queued keys still
    // counted.
    let o = s
        .observe(&[prio(key(5), 1), prio(key(1), 2)], 2)
        .await
        .unwrap();
    assert_eq!((o.queued, o.dropped), (0, 1));
    assert_eq!(pending_seen(&pool, key(5)).await, None);
    assert_eq!(pending_seen(&pool, key(1)).await, Some(3));
    // Below the ceiling (depth 4 < 2 x 3), priority keys pass the gate.
    let o = s
        .observe(&[prio(key(5), 1), obs(key(6), 1)], 3)
        .await
        .unwrap();
    assert_eq!((o.queued, o.dropped), (1, 1));
    // A limit of 0 admits nothing.
    let o = s.observe(&[prio(key(7), 1)], 0).await.unwrap();
    assert_eq!((o.queued, o.dropped), (0, 1));
    assert_eq!(s.pending_depth().await.unwrap(), 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn get_by_key_caps_the_path_bytes(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    // 100 paths of 4096 three-byte characters: about 1.2 MiB of path text.
    let path: String = "\u{20ac}".repeat(MAX_PATH_CHARS);
    let files: Vec<FileRow> = (0..100)
        .map(|_| FileRow {
            path: path.clone(),
            size: 1,
        })
        .collect();
    let t = NewTorrent {
        total_size: 100,
        file_count: 100,
        files,
        ..torrent(k, "wide")
    };
    s.complete(&k, &t).await.unwrap();
    let r = s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().unwrap();
    let bytes: usize = r.files.iter().map(|f| f.path.len()).sum();
    let cap = GET_BY_KEY_MAX_PATH_BYTES as usize;
    assert!(bytes <= cap, "{bytes} bytes of paths");
    assert_eq!(r.files.len(), cap / path.len());
    assert!(r.files_truncated);
    assert_eq!(r.file_count, 100);
    assert!(r.files.iter().all(|f| f.path == path));

    // A small list is returned whole and not marked.
    let k2 = key(2);
    s.complete(&k2, &torrent(k2, "small")).await.unwrap();
    let r = s.get_by_key(&AnyKey::V1OrDht(k2)).await.unwrap().unwrap();
    assert_eq!(r.files.len(), 2);
    assert!(!r.files_truncated);
}

#[sqlx::test(migrations = "./migrations")]
async fn pending_depth_uses_the_estimate_above_the_threshold(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let keys: Vec<Observation> = (0..10u8).map(|i| obs(key(i), 1)).collect();
    s.observe(&keys, NO_LIMIT).await.unwrap();
    assert_eq!(s.pending_depth().await.unwrap(), 10);

    // A small estimate still counts exactly.
    sqlx::query("ANALYZE pending").execute(&pool).await.unwrap();
    assert_eq!(s.pending_depth().await.unwrap(), 10);

    // A large estimate is returned as is, without counting. (The test role is
    // a superuser, so it can plant one.)
    let big = EXACT_COUNT_THRESHOLD * 5;
    sqlx::query("UPDATE pg_class SET reltuples = $1 WHERE oid = 'pending'::regclass")
        .bind(big as f32)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(s.pending_depth().await.unwrap(), big);
    // ... and it gates observe.
    let o = s.observe(&[obs(key(50), 1)], big).await.unwrap();
    assert_eq!(o.dropped, 1);
    let o = s.observe(&[obs(key(51), 1)], big + 1).await.unwrap();
    assert_eq!(o.queued, 1);

    // With a small estimate, a limit above the exact-count range never gates,
    // and a limit inside it is compared with the exact count.
    sqlx::query("UPDATE pg_class SET reltuples = 0 WHERE oid = 'pending'::regclass")
        .execute(&pool)
        .await
        .unwrap();
    // Ten keys, plus key(51); key(50) was dropped.
    assert_eq!(s.pending_depth().await.unwrap(), 11);
    for (limit, gated) in [
        (EXACT_COUNT_THRESHOLD + 2, false),
        (EXACT_COUNT_THRESHOLD, false),
        (12, false),
        (11, true),
        (1, true),
        (0, true),
        (-5, true),
    ] {
        assert_eq!(
            s.pending_depth_reaches(limit).await.unwrap(),
            gated,
            "limit {limit}"
        );
        assert_eq!(
            s.pending_depth().await.unwrap() >= limit,
            gated,
            "limit {limit}"
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn complete_is_idempotent_and_carries_seen_count(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(7);
    s.observe(&[obs(k, 4)], NO_LIMIT).await.unwrap();
    let discovered: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT discovered_at FROM pending")
            .fetch_one(&pool)
            .await
            .unwrap();

    let t = torrent(k, "Ubuntu");
    let id = s.complete(&k, &t).await.unwrap();
    let r1 = raw_torrent(&pool, id).await;
    assert_eq!(r1.seen_count, 4);
    assert_eq!(r1.first_seen_at, discovered);
    assert_eq!(r1.name, "Ubuntu");
    assert_eq!(r1.files, t.files);
    assert_eq!(r1.info_hash_v1, Some(k));
    assert_eq!(r1.piece_length, Some(16384));
    assert_eq!(
        (r1.hidden_at, r1.reviewed_at, r1.deleted_at),
        (None, None, None)
    );
    assert!(r1.change_seq > 0);

    // Twice: same row, same count, content refreshed, change_seq moved.
    let id2 = s
        .complete(
            &k,
            &NewTorrent {
                name: "Ubuntu 2".into(),
                ..t.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(id, id2);
    let r2 = raw_torrent(&pool, id).await;
    assert_eq!(r2.seen_count, 4);
    assert_eq!(r2.first_seen_at, r1.first_seen_at);
    assert_eq!(r2.name, "Ubuntu 2");
    assert!(r2.change_seq > r1.change_seq);
    assert_eq!(today(&pool).await.fetched, 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM torrents")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);

    // A key seen again after completion and re-queued (e.g. by a lost race)
    // adds its pending count on the next completion.
    sqlx::query("INSERT INTO pending (dht_key, seen_count) VALUES ($1, 10)")
        .bind(k.0.as_slice())
        .execute(&pool)
        .await
        .unwrap();
    s.complete(&k, &t).await.unwrap();
    assert_eq!(raw_torrent(&pool, id).await.seen_count, 14);
    assert_eq!(s.stats().await.unwrap().pending, 0);

    // The same v2 torrent under another DHT key (hybrid: v1 key vs truncated v2)
    // updates the existing row instead of violating uniqueness.
    let h = v2(3);
    let k1 = key(30);
    let hyb = NewTorrent {
        info_hash_v2: Some(h),
        ..torrent(k1, "hyb")
    };
    let hid = s.complete(&k1, &hyb).await.unwrap();
    let alt = NewTorrent {
        dht_key: h.truncated(),
        ..hyb.clone()
    };
    assert_eq!(s.complete(&h.truncated(), &alt).await.unwrap(), hid);

    // Validation.
    let bad = NewTorrent {
        name: "x".repeat(MAX_NAME_CHARS + 1),
        ..t.clone()
    };
    assert!(matches!(
        s.complete(&k, &bad).await,
        Err(StoreError::Invalid(_))
    ));
    let bad = NewTorrent {
        total_size: u64::MAX,
        ..t.clone()
    };
    assert!(matches!(
        s.complete(&k, &bad).await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        s.complete(&key(8), &t).await,
        Err(StoreError::Invalid(_))
    ));
    let many = NewTorrent {
        files: vec![
            FileRow {
                path: "p".into(),
                size: 1
            };
            MAX_STORED_FILES + 1
        ],
        ..t.clone()
    };
    assert!(matches!(
        s.complete(&k, &many).await,
        Err(StoreError::Invalid(_))
    ));
    // The full list is accepted.
    let full = torrent_with_files(key(31), MAX_STORED_FILES);
    let fid = s.complete(&key(31), &full).await.unwrap();
    assert_eq!(raw_torrent(&pool, fid).await.files.len(), MAX_STORED_FILES);
}

#[sqlx::test(migrations = "./migrations")]
async fn deny_tombstones_by_each_key_type(pool: PgPool) {
    let s = Store::from_pool(pool.clone());

    let a = key(1); // denied by DHT key
    let b_v1 = key(2); // hybrid stored under truncated v2, denied by v1
    let b_v2 = v2(2);
    let c_v2 = v2(3); // v2-only, denied by full v2 hash
    let d_v2 = v2(4); // hybrid stored under v1, denied by truncated v2 (20 bytes)
    let d_v1 = key(40);

    let ta = torrent(a, "alpha");
    let tb = NewTorrent {
        dht_key: b_v2.truncated(),
        info_hash_v1: Some(b_v1),
        info_hash_v2: Some(b_v2),
        ..torrent(b_v2.truncated(), "bravo")
    };
    let tc = torrent_v2(c_v2, "charlie");
    let td = NewTorrent {
        info_hash_v2: Some(d_v2),
        ..torrent(d_v1, "delta")
    };
    let live = key(50);
    let tl = torrent(live, "live");

    let ia = s.complete(&a, &ta).await.unwrap();
    let ib = s.complete(&tb.dht_key, &tb).await.unwrap();
    let ic = s.complete(&tc.dht_key, &tc).await.unwrap();
    let id = s.complete(&d_v1, &td).await.unwrap();
    let il = s.complete(&live, &tl).await.unwrap();

    let mark0 = s.high_water_mark().await.unwrap().unwrap();
    let rows = s.changes_since(0, mark0, 100).await.unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| r.visible));
    assert!(rows.windows(2).all(|w| w[0].change_seq < w[1].change_seq));
    assert_eq!(rows[0].files_text, "a/one.txt\nb/two.bin");
    assert_eq!(rows[0].name, "alpha");
    assert_eq!(rows[1].info_hash_v2, Some(b_v2));

    // Before: all visible by every key form.
    assert!(
        s.get_by_key(&AnyKey::V1OrDht(b_v1))
            .await
            .unwrap()
            .is_some()
    );
    assert!(s.get_by_key(&AnyKey::V2(c_v2)).await.unwrap().is_some());
    assert_eq!(s.get_many(&[il, ia, ib, ic, id]).await.unwrap().len(), 5);

    // A pending row under a denied key is removed.
    let pk = key(60);
    s.observe(&[obs(pk, 1)], NO_LIMIT).await.unwrap();

    let out = s
        .deny(a.as_bytes(), DenyReason::Dmca, Some("notice 1"), "admin")
        .await
        .unwrap();
    assert_eq!(
        out,
        DenyOutcome {
            newly_denied: true,
            tombstoned: 1
        }
    );
    let out = s
        .deny(a.as_bytes(), DenyReason::Other, None, "admin")
        .await
        .unwrap();
    assert_eq!(
        out,
        DenyOutcome {
            newly_denied: false,
            tombstoned: 1
        },
        "idempotent"
    );
    assert_eq!(
        s.deny(b_v1.as_bytes(), DenyReason::Csam, None, "admin")
            .await
            .unwrap()
            .tombstoned,
        1
    );
    assert_eq!(
        s.deny(c_v2.as_bytes(), DenyReason::Abuse, None, "admin")
            .await
            .unwrap()
            .tombstoned,
        1
    );
    assert_eq!(
        s.deny(
            d_v2.truncated().as_bytes(),
            DenyReason::Other,
            None,
            "admin"
        )
        .await
        .unwrap()
        .tombstoned,
        1
    );
    assert_eq!(
        s.deny(pk.as_bytes(), DenyReason::Private, None, "crawler")
            .await
            .unwrap()
            .tombstoned,
        0
    );
    assert!(matches!(
        s.deny(&[1, 2, 3], DenyReason::Other, None, "a").await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        s.deny(a.as_bytes(), DenyReason::Other, None, "").await,
        Err(StoreError::Invalid(_))
    ));

    for idn in [ia, ib, ic, id] {
        let r = raw_torrent(&pool, idn).await;
        assert!(r.deleted_at.is_some());
        assert_eq!(r.name, "");
        assert!(r.files.is_empty());
    }
    assert_eq!(s.stats().await.unwrap().pending, 0);

    // get_by_key: denied rows are gone under every key form.
    for k in [
        AnyKey::V1OrDht(a),
        AnyKey::V1OrDht(b_v1),
        AnyKey::V1OrDht(b_v2.truncated()),
        AnyKey::V2(b_v2),
        AnyKey::V2(c_v2),
        AnyKey::V1OrDht(c_v2.truncated()),
        AnyKey::V1OrDht(d_v1),
        AnyKey::V2(d_v2),
    ] {
        assert!(s.get_by_key(&k).await.unwrap().is_none(), "{k:?}");
    }
    assert_eq!(
        s.get_by_key(&AnyKey::V1OrDht(live))
            .await
            .unwrap()
            .unwrap()
            .id,
        il
    );

    // get_many keeps order and drops invisible rows.
    let many = s.get_many(&[il, ia, 9999, ib, il]).await.unwrap();
    assert_eq!(many.iter().map(|r| r.id).collect::<Vec<_>>(), vec![il, il]);
    assert!(s.get_many(&vec![1; MAX_GET_MANY + 1]).await.is_err());
    assert!(s.get_many(&[]).await.unwrap().is_empty());

    // The change feed reports the tombstones as invisible, with no text.
    let mark1 = s.high_water_mark().await.unwrap().unwrap();
    assert!(mark1 > mark0);
    let changed = s.changes_since(mark0, mark1, 100).await.unwrap();
    let ids: HashSet<i64> = changed.iter().map(|r| r.id).collect();
    assert_eq!(ids, HashSet::from([ia, ib, ic, id]));
    assert!(changed.iter().all(|r| !r.visible));
    assert!(
        changed
            .iter()
            .all(|r| r.files_text.is_empty() && r.name.is_empty())
    );
    assert!(s.changes_since(mark1, mark1, 100).await.unwrap().is_empty());
    assert!(s.changes_since(0, mark1, 0).await.unwrap().is_empty());
    assert_eq!(s.changes_since(0, mark1, 2).await.unwrap().len(), 2);

    // is_denied by every form.
    assert!(s.is_denied(&[a.as_bytes()]).await.unwrap());
    assert!(
        s.is_denied(&[key(77).as_bytes(), b_v1.as_bytes()])
            .await
            .unwrap()
    );
    assert!(s.is_denied(&[c_v2.truncated().as_bytes()]).await.unwrap());
    assert!(s.is_denied(&[d_v2.as_bytes()]).await.unwrap());
    assert!(
        !s.is_denied(&[live.as_bytes(), key(77).as_bytes()])
            .await
            .unwrap()
    );
    assert!(!s.is_denied(&[]).await.unwrap());

    // Completing a denied torrent fails and drops the queue row.
    sqlx::query("INSERT INTO pending (dht_key) VALUES ($1)")
        .bind(a.0.as_slice())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(s.complete(&a, &ta).await, Err(StoreError::Denied)));
    assert_eq!(s.stats().await.unwrap().pending, 0);
    assert!(raw_torrent(&pool, ia).await.deleted_at.is_some());

    // Denylist listing, stats and audit.
    let listed = s.list_denied(100, 0).await.unwrap();
    assert_eq!(listed.len(), 5);
    let ea = listed.iter().find(|e| e.key == AnyKey::V1OrDht(a)).unwrap();
    assert_eq!(
        (ea.reason, ea.note.as_deref(), ea.created_by.as_str()),
        (DenyReason::Dmca, Some("notice 1"), "admin")
    );
    assert!(listed.iter().any(|e| e.key == AnyKey::V2(c_v2)));
    assert_eq!(s.list_denied(2, 4).await.unwrap().len(), 1);
    assert_eq!(today(&pool).await.blocked, 5);
    let st = s.stats().await.unwrap();
    assert_eq!((st.torrents, st.denylisted), (1, 5));
    assert_eq!(audit_count(&pool, "deny").await, 6);

    // Undeny removes the entry but leaves the tombstone; a new fetch restores it.
    assert!(s.undeny(a.as_bytes(), "admin").await.unwrap());
    assert!(!s.undeny(a.as_bytes(), "admin").await.unwrap());
    assert!(!s.is_denied(&[a.as_bytes()]).await.unwrap());
    assert!(s.get_by_key(&AnyKey::V1OrDht(a)).await.unwrap().is_none());
    assert_eq!(s.complete(&a, &ta).await.unwrap(), ia);
    let restored = s.get_by_key(&AnyKey::V1OrDht(a)).await.unwrap().unwrap();
    assert_eq!(restored.name, "alpha");
    assert_eq!(restored.files.len(), 2);
    assert_eq!(audit_count(&pool, "undeny").await, 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn high_water_mark_waits_for_writers(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let id = s.complete(&key(1), &torrent(key(1), "one")).await.unwrap();
    let committed = s.high_water_mark().await.unwrap().unwrap();
    assert_eq!(committed, change_seq_of(&pool, id).await);

    // A writer holds the shared lock and an uncommitted sequence number, for
    // longer than HWM_LOCK_TIMEOUT.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(CHANGE_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .unwrap();
    let pending_seq: i64 = sqlx::query_scalar(
        "UPDATE torrents SET change_seq = nextval('change_seq') WHERE id = $1 RETURNING change_seq",
    )
    .bind(id)
    .fetch_one(&mut *writer)
    .await
    .unwrap();
    assert!(pending_seq > committed);

    // Other readers still see the old row.
    let visible = s.changes_since(committed, i64::MAX, 10).await.unwrap();
    assert!(visible.is_empty());

    // With a long timeout, the mark waits for the writer to finish.
    let hwm = tokio::spawn({
        let s = s.clone();
        async move {
            s.high_water_mark_within(Duration::from_secs(60))
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(HWM_LOCK_TIMEOUT * 3).await;
    assert!(!hwm.is_finished(), "the mark must wait for the writer");

    writer.commit().await.unwrap();
    let mark = tokio::time::timeout(Duration::from_secs(10), hwm)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mark, Some(pending_seq));
    let rows = s.changes_since(committed, pending_seq, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].change_seq, pending_seq);

    // A rolled-back writer leaves a gap, which is harmless.
    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(CHANGE_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .unwrap();
    let lost: i64 = sqlx::query_scalar("SELECT nextval('change_seq')")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    writer.rollback().await.unwrap();
    assert_eq!(s.high_water_mark().await.unwrap(), Some(lost));
    assert!(
        s.changes_since(pending_seq, lost, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn high_water_mark_gives_up_on_a_long_writer(pool: PgPool) {
    // One connection, so the checks below run on the one the mark used.
    let single = session_pool(&pool, 1).await;
    let s = Store::from_pool(single.clone());
    let id = s.complete(&key(1), &torrent(key(1), "one")).await.unwrap();
    let before = s.high_water_mark().await.unwrap().unwrap();

    let mut writer = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock_shared($1)")
        .bind(CHANGE_LOCK_KEY)
        .execute(&mut *writer)
        .await
        .unwrap();
    let seq: i64 = sqlx::query_scalar(
        "UPDATE torrents SET change_seq = nextval('change_seq') WHERE id = $1 RETURNING change_seq",
    )
    .bind(id)
    .fetch_one(&mut *writer)
    .await
    .unwrap();

    let started = Instant::now();
    assert_eq!(s.high_water_mark().await.unwrap(), None);
    let waited = started.elapsed();
    assert!(waited >= HWM_LOCK_TIMEOUT / 2, "gave up after {waited:?}");
    assert!(waited < Duration::from_secs(10), "waited {waited:?}");

    // The timeout was local to the transaction, and the connection is usable.
    let lock_timeout: String = sqlx::query_scalar("SHOW lock_timeout")
        .fetch_one(&single)
        .await
        .unwrap();
    assert_eq!(lock_timeout, "0");
    let in_tx: bool = sqlx::query_scalar("SELECT pg_current_xact_id_if_assigned() IS NOT NULL")
        .fetch_one(&single)
        .await
        .unwrap();
    assert!(!in_tx);

    writer.commit().await.unwrap();
    let after = s.high_water_mark().await.unwrap().unwrap();
    assert_eq!(after, seq);
    assert!(after > before);
    single.close().await;
}

#[sqlx::test(migrations = "./migrations")]
async fn change_feed_pages_within_the_byte_budget(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let mut ids = Vec::new();
    for i in 1..=3u8 {
        ids.push(
            s.complete(&key(i), &torrent_with_files(key(i), usize::from(i)))
                .await
                .unwrap(),
        );
    }
    let mark = s.high_water_mark().await.unwrap().unwrap();
    let all = s.changes_since(0, mark, 100).await.unwrap();
    assert_eq!(all.iter().map(|r| r.id).collect::<Vec<_>>(), ids);
    assert_eq!(all[0].files_text, "dir/f000");
    assert_eq!(all[2].files_text, "dir/f000\ndir/f001\ndir/f002");
    assert_eq!(all[2].file_count, 3);

    // A budget smaller than one row still makes progress, one row at a time.
    let mut after = 0;
    let mut seen = Vec::new();
    loop {
        let page = s.changes_within(after, mark, 100, 1).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        after = page[0].change_seq;
        seen.push(page[0].id);
    }
    assert_eq!(seen, ids);

    // A budget just above the first row's size admits exactly two rows.
    let first: i64 = sqlx::query_scalar(
        "SELECT octet_length(name)::bigint + octet_length(files::text) FROM torrents WHERE id = $1",
    )
    .bind(ids[0])
    .fetch_one(&pool)
    .await
    .unwrap();
    let page = s.changes_within(0, mark, 100, first + 1).await.unwrap();
    assert_eq!(page.len(), 2);
    let page = s.changes_within(0, mark, 100, first).await.unwrap();
    assert_eq!(page.len(), 1);

    // Pages hold at most MAX_FEED_PAGE rows.
    let cap = usize::try_from(MAX_FEED_PAGE).unwrap();
    insert_bare_torrents(&pool, MAX_FEED_PAGE + 1).await;
    let mark = s.high_water_mark().await.unwrap().unwrap();
    assert_eq!(s.changes_since(0, mark, i64::MAX).await.unwrap().len(), cap);
    assert_eq!(s.scan_live(0, i64::MAX).await.unwrap().len(), cap);
}

#[sqlx::test(migrations = "./migrations")]
async fn scan_live_pages_in_id_order(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let t1 = s.complete(&key(1), &torrent(key(1), "one")).await.unwrap();
    let t2 = s.complete(&key(2), &torrent(key(2), "two")).await.unwrap();
    let t3 = s
        .complete(&key(3), &torrent(key(3), "three"))
        .await
        .unwrap();
    let h = v2(4);
    let t4 = s
        .complete(&h.truncated(), &torrent_v2(h, "four"))
        .await
        .unwrap();
    let t5 = s
        .complete(&key(5), &torrent_with_files(key(5), 3))
        .await
        .unwrap();
    // Hidden rows are scanned; tombstoned rows are not.
    assert!(s.submit_report(&csam(key(2))).await.unwrap().hidden);
    s.deny(key(3).as_bytes(), DenyReason::Dmca, None, "admin")
        .await
        .unwrap();

    let p1 = s.scan_live(0, 2).await.unwrap();
    assert_eq!(p1.iter().map(|r| r.id).collect::<Vec<_>>(), vec![t1, t2]);
    assert_eq!(
        p1[0],
        LiveRow {
            id: t1,
            dht_key: key(1),
            name: "one".into(),
            paths: vec!["a/one.txt".into(), "b/two.bin".into()],
        }
    );
    let p2 = s.scan_live(p1[1].id, 2).await.unwrap();
    assert_eq!(p2.iter().map(|r| r.id).collect::<Vec<_>>(), vec![t4, t5]);
    assert_eq!(p2[0].dht_key, h.truncated());
    assert_eq!(p2[1].paths, vec!["dir/f000", "dir/f001", "dir/f002"]);
    assert!(s.scan_live(t5, 2).await.unwrap().is_empty());
    assert!(s.scan_live(0, 0).await.unwrap().is_empty());
    assert!(s.scan_live(0, -1).await.unwrap().is_empty());

    // A tiny byte budget still returns one row per page.
    let mut after = 0;
    let mut seen = Vec::new();
    loop {
        let page = s.scan_live_within(after, 10, 1).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        after = page[0].id;
        seen.push(after);
    }
    assert_eq!(seen, vec![t1, t2, t4, t5]);
    assert!(!seen.contains(&t3));

    // A torrent without files has no paths.
    sqlx::query("UPDATE torrents SET files = '[]' WHERE id = $1")
        .bind(t1)
        .execute(&pool)
        .await
        .unwrap();
    assert!(s.scan_live(0, 1).await.unwrap()[0].paths.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn submit_report_hides_once_and_audits(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    let tid = s.complete(&k, &torrent(k, "reported")).await.unwrap();
    let seq0 = change_seq_of(&pool, tid).await;

    let first = s.submit_report(&csam(k)).await.unwrap();
    assert!(first.hidden);
    assert!(!first.budget_exhausted);
    assert_eq!(report_torrent(&pool, first.report_id).await, Some(tid));
    assert!(s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_none());
    let hidden = raw_torrent(&pool, tid).await;
    assert!(hidden.hidden_at.is_some());
    assert!(hidden.deleted_at.is_none() && hidden.reviewed_at.is_none());
    assert!(hidden.change_seq > seq0);
    assert_eq!(audit_count(&pool, "auto-hide").await, 1);
    let audit =
        sqlx::query("SELECT actor, subject, detail FROM audit_log WHERE action = 'auto-hide'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit.get::<String, _>("actor"), "web");
    assert_eq!(audit.get::<String, _>("subject"), k.to_hex());
    assert_eq!(
        audit.get::<serde_json::Value, _>("detail"),
        serde_json::json!({"report_id": first.report_id, "torrent_id": tid})
    );

    // The index sees the hide.
    let mark = s.high_water_mark().await.unwrap().unwrap();
    let feed = s.changes_since(seq0, mark, 10).await.unwrap();
    assert_eq!(feed.len(), 1);
    assert!(!feed[0].visible);

    // A second CSAM report is stored and linked, but neither hides again nor
    // uses the budget.
    let second = s.submit_report(&csam(k)).await.unwrap();
    assert_eq!(
        second,
        SubmitOutcome {
            report_id: second.report_id,
            hidden: false,
            budget_exhausted: false
        }
    );
    assert!(second.report_id > first.report_id);
    assert_eq!(report_torrent(&pool, second.report_id).await, Some(tid));
    let again = raw_torrent(&pool, tid).await;
    assert_eq!(again.hidden_at, hidden.hidden_at);
    assert_eq!(again.change_seq, hidden.change_seq);
    assert_eq!(audit_count(&pool, "auto-hide").await, 1);

    // Other reasons never hide.
    let k2 = key(2);
    let t2 = s.complete(&k2, &torrent(k2, "other")).await.unwrap();
    for reason in [
        ReportReason::Copyright,
        ReportReason::Malware,
        ReportReason::Other,
    ] {
        let out = s
            .submit_report(&NewReport {
                contact: Some("me@example.org".into()),
                ..report(AnyKey::V1OrDht(k2), reason, "why")
            })
            .await
            .unwrap();
        assert!(!out.hidden && !out.budget_exhausted);
        assert_eq!(report_torrent(&pool, out.report_id).await, Some(t2));
    }
    assert!(s.get_by_key(&AnyKey::V1OrDht(k2)).await.unwrap().is_some());

    let r = s.get_report(first.report_id).await.unwrap().unwrap();
    assert_eq!(
        (r.torrent_id, r.key, r.reason, r.status),
        (
            Some(tid),
            AnyKey::V1OrDht(k),
            ReportReason::Csam,
            ReportStatus::Open
        )
    );
    assert_eq!(s.stats().await.unwrap().open_reports, 5);
}

#[sqlx::test(migrations = "./migrations")]
async fn submit_report_links_by_every_key_form(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let other = |k: AnyKey| report(k, ReportReason::Other, "");

    // A hybrid stored under its v1 key.
    let (k1, h1) = (key(1), v2(1));
    let hybrid = s
        .complete(
            &k1,
            &NewTorrent {
                info_hash_v2: Some(h1),
                ..torrent(k1, "hybrid")
            },
        )
        .await
        .unwrap();
    // A v2-only torrent stored under its truncated hash.
    let h2 = v2(2);
    let v2_only = s
        .complete(&h2.truncated(), &torrent_v2(h2, "v2"))
        .await
        .unwrap();
    // A v1 torrent stored under another DHT key than its v1 hash.
    let (k3, v1_3) = (key(3), key(33));
    let aliased = s
        .complete(
            &k3,
            &NewTorrent {
                info_hash_v1: Some(v1_3),
                ..torrent(k3, "aliased")
            },
        )
        .await
        .unwrap();

    for (k, want) in [
        (AnyKey::V1OrDht(k1), Some(hybrid)),
        (AnyKey::V2(h1), Some(hybrid)),
        (AnyKey::V1OrDht(h1.truncated()), Some(hybrid)),
        (AnyKey::V2(h2), Some(v2_only)),
        (AnyKey::V1OrDht(h2.truncated()), Some(v2_only)),
        (AnyKey::V1OrDht(k3), Some(aliased)),
        (AnyKey::V1OrDht(v1_3), Some(aliased)),
        (AnyKey::V1OrDht(key(99)), None),
        (AnyKey::V2(v2(99)), None),
        // A 32-byte key whose prefix is a v1 key is not that torrent.
        (
            AnyKey::V2(InfoHashV2({
                let mut b = [0u8; 32];
                b[..20].copy_from_slice(k1.as_bytes());
                b
            })),
            None,
        ),
    ] {
        let out = s.submit_report(&other(k)).await.unwrap();
        assert_eq!(report_torrent(&pool, out.report_id).await, want, "{k:?}");
        let stored = s.get_report(out.report_id).await.unwrap().unwrap();
        assert_eq!(stored.key, k);
    }

    // Unknown keys are stored unlinked and hide nothing, even for CSAM.
    let out = s.submit_report(&csam(key(98))).await.unwrap();
    assert!(!out.hidden && !out.budget_exhausted);
    assert_eq!(report_torrent(&pool, out.report_id).await, None);

    // Tombstoned torrents are not linked or hidden.
    s.deny(k3.as_bytes(), DenyReason::Dmca, None, "admin")
        .await
        .unwrap();
    let out = s.submit_report(&csam(k3)).await.unwrap();
    assert!(!out.hidden);
    assert_eq!(report_torrent(&pool, out.report_id).await, None);
    assert!(raw_torrent(&pool, aliased).await.hidden_at.is_none());

    // A torrent covered by the denylist but not tombstoned is not linked.
    sqlx::query("INSERT INTO denylist (key, reason, created_by) VALUES ($1, 'other', 'test')")
        .bind(h2.as_bytes().as_slice())
        .execute(&pool)
        .await
        .unwrap();
    let out = s.submit_report(&csam(h2.truncated())).await.unwrap();
    assert!(!out.hidden);
    assert_eq!(report_torrent(&pool, out.report_id).await, None);
    assert!(raw_torrent(&pool, v2_only).await.hidden_at.is_none());
    assert_eq!(audit_count(&pool, "auto-hide").await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn submit_report_respects_the_hourly_budget(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    assert!(s.set_setting(SETTING_AUTOHIDE_PER_HOUR, "2").await.unwrap());
    let mut ids = Vec::new();
    for i in 1..=6u8 {
        ids.push(s.complete(&key(i), &torrent(key(i), "t")).await.unwrap());
    }

    assert!(s.submit_report(&csam(key(1))).await.unwrap().hidden);
    assert!(s.submit_report(&csam(key(2))).await.unwrap().hidden);
    let third = s.submit_report(&csam(key(3))).await.unwrap();
    assert!(!third.hidden);
    assert!(third.budget_exhausted);
    assert_eq!(report_torrent(&pool, third.report_id).await, Some(ids[2]));
    assert!(
        s.get_by_key(&AnyKey::V1OrDht(key(3)))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(audit_count(&pool, "auto-hide").await, 2);

    // A report that could not hide anyway does not report the budget.
    let out = s.submit_report(&csam(key(1))).await.unwrap();
    assert!(!out.hidden && !out.budget_exhausted);
    let out = s
        .submit_report(&report(
            AnyKey::V1OrDht(key(4)),
            ReportReason::Copyright,
            "",
        ))
        .await
        .unwrap();
    assert!(!out.budget_exhausted);

    // Auto-hides older than an hour no longer count.
    sqlx::query(
        "UPDATE audit_log SET at = now() - interval '61 minutes' WHERE action = 'auto-hide'",
    )
    .execute(&pool)
    .await
    .unwrap();
    // key(3) already has an open CSAM report (the one the budget stopped),
    // so another one does not hide it: only the first open report hides.
    let out = s.submit_report(&csam(key(3))).await.unwrap();
    assert!(!out.hidden && !out.budget_exhausted);
    assert!(s.submit_report(&csam(key(4))).await.unwrap().hidden);
    assert_eq!(audit_count(&pool, "auto-hide").await, 3);

    // A budget of 0 disables auto-hiding.
    s.set_setting(SETTING_AUTOHIDE_PER_HOUR, "0").await.unwrap();
    let out = s.submit_report(&csam(key(5))).await.unwrap();
    assert!(!out.hidden && out.budget_exhausted);

    // Without the setting, the default applies.
    sqlx::query("DELETE FROM settings WHERE key = $1")
        .bind(SETTING_AUTOHIDE_PER_HOUR)
        .execute(&pool)
        .await
        .unwrap();
    assert!(s.submit_report(&csam(key(6))).await.unwrap().hidden);
    assert_eq!(audit_count(&pool, "auto-hide").await, 4);
}

#[sqlx::test(migrations = "./migrations")]
async fn submit_report_budget_holds_under_concurrency(pool: PgPool) {
    const TORRENTS: u8 = 12;
    const BUDGET: usize = 3;
    let s = Store::from_pool(pool.clone());
    s.set_setting(SETTING_AUTOHIDE_PER_HOUR, &BUDGET.to_string())
        .await
        .unwrap();
    for i in 1..=TORRENTS {
        s.complete(&key(i), &torrent(key(i), "t")).await.unwrap();
    }
    let mut tasks = Vec::new();
    for i in 1..=TORRENTS {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.submit_report(&csam(key(i))).await.unwrap()
        }));
    }
    let mut outs = Vec::new();
    for t in tasks {
        outs.push(t.await.unwrap());
    }
    assert_eq!(outs.iter().filter(|o| o.hidden).count(), BUDGET);
    assert_eq!(
        outs.iter().filter(|o| o.budget_exhausted).count(),
        usize::from(TORRENTS) - BUDGET
    );
    assert_eq!(audit_count(&pool, "auto-hide").await, BUDGET as i64);
    let hidden: i64 =
        sqlx::query_scalar("SELECT count(*) FROM torrents WHERE hidden_at IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(hidden, BUDGET as i64);

    // Many CSAM reports about one torrent hide it exactly once.
    let k = key(100);
    s.complete(&k, &torrent(k, "one")).await.unwrap();
    s.set_setting(SETTING_AUTOHIDE_PER_HOUR, "100")
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.submit_report(&csam(k)).await.unwrap()
        }));
    }
    let mut hides = 0;
    for t in tasks {
        if t.await.unwrap().hidden {
            hides += 1;
        }
    }
    assert_eq!(hides, 1);
    assert_eq!(audit_count(&pool, "auto-hide").await, BUDGET as i64 + 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn submit_report_refuses_beyond_the_open_cap(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    s.set_setting(SETTING_OPEN_REPORTS_CAP, "3").await.unwrap();
    let k = key(1);
    s.complete(&k, &torrent(k, "t")).await.unwrap();
    let other = || report(AnyKey::V1OrDht(key(50)), ReportReason::Other, "spam?");

    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(s.submit_report(&other()).await.unwrap().report_id);
    }
    assert!(matches!(
        s.submit_report(&other()).await,
        Err(StoreError::ReportsFull)
    ));
    // A refused CSAM report neither stores nor hides anything.
    assert!(matches!(
        s.submit_report(&csam(k)).await,
        Err(StoreError::ReportsFull)
    ));
    assert!(s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_some());
    assert_eq!(s.stats().await.unwrap().open_reports, 3);
    assert_eq!(audit_count(&pool, "auto-hide").await, 0);

    // The raw error carries the documented SQLSTATE.
    let err = sqlx::query("SELECT * FROM submit_report($1, 'other', '', NULL)")
        .bind(k.as_bytes().as_slice())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(code_of(&err).as_deref(), Some(SQLSTATE_REPORTS_FULL));

    // Resolved reports no longer count.
    s.resolve_report(ids[0], ReportAction::Dismiss, None, "admin")
        .await
        .unwrap();
    assert!(s.submit_report(&csam(k)).await.unwrap().hidden);
    assert!(matches!(
        s.submit_report(&other()).await,
        Err(StoreError::ReportsFull)
    ));

    // A cap of 0 refuses everything; without the setting the default applies.
    s.set_setting(SETTING_OPEN_REPORTS_CAP, "0").await.unwrap();
    s.resolve_report(ids[1], ReportAction::Dismiss, None, "admin")
        .await
        .unwrap();
    assert!(matches!(
        s.submit_report(&other()).await,
        Err(StoreError::ReportsFull)
    ));
    sqlx::query("DELETE FROM settings WHERE key = $1")
        .bind(SETTING_OPEN_REPORTS_CAP)
        .execute(&pool)
        .await
        .unwrap();
    s.submit_report(&other()).await.unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn submit_report_validates_its_arguments(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = AnyKey::V1OrDht(key(1));

    // Limits are in characters.
    let at_limit = NewReport {
        contact: Some("é".repeat(REPORT_CONTACT_MAX_CHARS)),
        ..report(
            k,
            ReportReason::Other,
            &"é".repeat(REPORT_MESSAGE_MAX_CHARS),
        )
    };
    let id = s.submit_report(&at_limit).await.unwrap().report_id;
    let stored = s.get_report(id).await.unwrap().unwrap();
    assert_eq!(stored.message, at_limit.message);
    assert_eq!(stored.contact, at_limit.contact);

    let long = report(
        k,
        ReportReason::Csam,
        &"m".repeat(REPORT_MESSAGE_MAX_CHARS + 1),
    );
    assert!(matches!(
        s.submit_report(&long).await,
        Err(StoreError::Invalid(_))
    ));
    let long = NewReport {
        contact: Some("c".repeat(REPORT_CONTACT_MAX_CHARS + 1)),
        ..report(k, ReportReason::Csam, "")
    };
    assert!(matches!(
        s.submit_report(&long).await,
        Err(StoreError::Invalid(_))
    ));

    // The function checks the same, for callers that bypass this crate.
    let long_message = "m".repeat(REPORT_MESSAGE_MAX_CHARS + 1);
    let long_contact = "c".repeat(REPORT_CONTACT_MAX_CHARS + 1);
    let k20 = vec![1u8; 20];
    // (key, reason, message, contact)
    type RawArgs<'a> = (
        Option<Vec<u8>>,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );
    let bad: [RawArgs<'_>; 8] = [
        (None, Some("other"), Some(""), None),
        (Some(vec![1u8; 1]), Some("other"), Some(""), None),
        (Some(vec![1u8; 21]), Some("other"), Some(""), None),
        (Some(k20.clone()), None, Some(""), None),
        (Some(k20.clone()), Some("spam"), Some(""), None),
        (Some(k20.clone()), Some("csam"), None, None),
        (Some(k20.clone()), Some("csam"), Some(&long_message), None),
        (
            Some(k20.clone()),
            Some("other"),
            Some(""),
            Some(&long_contact),
        ),
    ];
    for (key_bytes, reason, message, contact) in bad {
        let err = sqlx::query("SELECT * FROM submit_report($1, $2, $3, $4)")
            .bind(key_bytes.as_deref())
            .bind(reason)
            .bind(message)
            .bind(contact)
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(
            code_of(&err).as_deref(),
            Some(SQLSTATE_INVALID_PARAMETER),
            "{reason:?} {err}"
        );
        assert!(
            matches!(submit_error(err), StoreError::Invalid(m) if m.starts_with("submit_report: "))
        );
    }
    let reports: i64 = sqlx::query_scalar("SELECT count(*) FROM reports")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(reports, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn submit_report_takes_the_change_lock_for_csam_only(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    s.complete(&k, &torrent(k, "t")).await.unwrap();

    // The indexer's exclusive lock blocks CSAM reports (they may bump
    // change_seq) but not others.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(CHANGE_LOCK_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '100ms'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query("SELECT * FROM submit_report($1, 'csam', '', NULL)")
        .bind(k.as_bytes().as_slice())
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(code_of(&err).as_deref(), Some(SQLSTATE_LOCK_NOT_AVAILABLE));
    tx.rollback().await.unwrap();

    let out = s
        .submit_report(&report(AnyKey::V1OrDht(k), ReportReason::Malware, ""))
        .await
        .unwrap();
    assert!(!out.hidden);

    holder.rollback().await.unwrap();
    assert!(s.submit_report(&csam(k)).await.unwrap().hidden);
}

#[sqlx::test(migrations = "./migrations")]
async fn reports_and_resolution(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let k = key(1);
    let tid = s.complete(&k, &torrent(k, "reported")).await.unwrap();

    // A copyright report does not hide.
    let r_copy = s
        .submit_report(&NewReport {
            contact: Some("me@example.org".into()),
            ..report(AnyKey::V1OrDht(k), ReportReason::Copyright, "mine")
        })
        .await
        .unwrap()
        .report_id;
    assert!(s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_some());

    // The first CSAM report hides; the second is linked to the same torrent.
    let r1 = s.submit_report(&csam(k)).await.unwrap();
    let r2 = s.submit_report(&csam(k)).await.unwrap();
    assert!(r1.hidden && !r2.hidden);
    let (r1, r2) = (r1.report_id, r2.report_id);
    assert!(s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_none());

    // Report on an unknown v2 key.
    let r_v2 = s
        .submit_report(&report(AnyKey::V2(v2(5)), ReportReason::Malware, ""))
        .await
        .unwrap()
        .report_id;

    let open = s.list_reports(Some(ReportStatus::Open), 100).await.unwrap();
    assert_eq!(
        open.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![r_copy, r1, r2, r_v2]
    );
    assert_eq!(open[0].contact.as_deref(), Some("me@example.org"));
    assert_eq!(open[0].message, "mine");
    assert_eq!(open[3].key, AnyKey::V2(v2(5)));
    assert_eq!(open[3].reason, ReportReason::Malware);
    assert_eq!(open[3].torrent_id, None);
    assert_eq!(s.list_reports(None, 2).await.unwrap().len(), 2);
    assert!(s.list_reports(None, 0).await.unwrap().is_empty());
    assert_eq!(s.stats().await.unwrap().open_reports, 4);

    // Dismissing one CSAM report keeps the torrent hidden (another is open),
    // but marks it reviewed.
    let seq = change_seq_of(&pool, tid).await;
    s.resolve_report(r1, ReportAction::Dismiss, Some("not csam"), "admin")
        .await
        .unwrap();
    assert!(s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_none());
    let row = raw_torrent(&pool, tid).await;
    assert!(row.reviewed_at.is_some() && row.hidden_at.is_some());
    assert_eq!(row.change_seq, seq, "reviewed_at alone is not a change");
    // Resolving twice fails.
    assert!(matches!(
        s.resolve_report(r1, ReportAction::Dismiss, None, "admin").await,
        Err(StoreError::ReportNotOpen(id)) if id == r1
    ));
    assert!(matches!(
        s.resolve_report(9999, ReportAction::Dismiss, None, "admin")
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        s.resolve_report(r2, ReportAction::Dismiss, None, "").await,
        Err(StoreError::Invalid(_))
    ));

    // Dismissing the last one un-hides.
    s.resolve_report(r2, ReportAction::Dismiss, None, "admin")
        .await
        .unwrap();
    let visible = s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().unwrap();
    assert!(visible.reviewed_at.is_some());
    let json = serde_json::to_value(&visible).unwrap();
    assert!(json.get("reviewed_at").is_none(), "{json}");
    assert_eq!(json["name"], "reported");
    assert!(visible.hidden_at.is_none());
    assert!(visible.change_seq > seq);
    let mark = s.high_water_mark().await.unwrap().unwrap();
    let feed = s.changes_since(seq, mark, 10).await.unwrap();
    assert_eq!(feed.len(), 1);
    assert!(feed[0].visible);

    let r = s.get_report(r2).await.unwrap().unwrap();
    assert_eq!(r.status, ReportStatus::Dismissed);
    assert_eq!(r.resolved_by.as_deref(), Some("admin"));
    assert!(r.resolved_at.is_some());
    assert!(s.get_report(12345).await.unwrap().is_none());

    // A reviewed torrent is not hidden again automatically.
    let r3 = s.submit_report(&csam(k)).await.unwrap();
    assert!(!r3.hidden && !r3.budget_exhausted);
    assert_eq!(report_torrent(&pool, r3.report_id).await, Some(tid));
    assert!(s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_some());
    assert_eq!(audit_count(&pool, "auto-hide").await, 1);

    // Dismissing a report about an unhidden torrent only marks it reviewed.
    let k2 = key(2);
    let t2 = s.complete(&k2, &torrent(k2, "fine")).await.unwrap();
    let seq2 = change_seq_of(&pool, t2).await;
    let r4 = s
        .submit_report(&report(AnyKey::V1OrDht(k2), ReportReason::Other, ""))
        .await
        .unwrap()
        .report_id;
    s.resolve_report(r4, ReportAction::Dismiss, None, "admin")
        .await
        .unwrap();
    let row = raw_torrent(&pool, t2).await;
    assert!(row.reviewed_at.is_some() && row.hidden_at.is_none());
    assert_eq!(row.change_seq, seq2);
    assert!(!s.submit_report(&csam(k2)).await.unwrap().hidden);

    // Deny via the copyright report: tombstone, denylist, actioned. The open
    // CSAM report r3 stays open for review.
    s.resolve_report(
        r_copy,
        ReportAction::Deny(DenyReason::Dmca),
        Some("DMCA #1"),
        "admin",
    )
    .await
    .unwrap();
    assert!(s.get_by_key(&AnyKey::V1OrDht(k)).await.unwrap().is_none());
    assert!(raw_torrent(&pool, tid).await.deleted_at.is_some());
    assert!(s.is_denied(&[k.as_bytes()]).await.unwrap());
    let r = s.get_report(r_copy).await.unwrap().unwrap();
    assert_eq!(r.status, ReportStatus::Actioned);
    assert_eq!(r.resolution_note.as_deref(), Some("DMCA #1"));
    let d = s.list_denied(10, 0).await.unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].reason, DenyReason::Dmca);

    // Deny by a report about a v2 key with no torrent.
    s.resolve_report(r_v2, ReportAction::Deny(DenyReason::Abuse), None, "admin")
        .await
        .unwrap();
    assert!(s.is_denied(&[v2(5).truncated().as_bytes()]).await.unwrap());

    // Dismissing a report about a tombstoned torrent is harmless.
    s.resolve_report(r3.report_id, ReportAction::Dismiss, None, "admin")
        .await
        .unwrap();
    assert!(raw_torrent(&pool, tid).await.deleted_at.is_some());

    assert_eq!(
        s.list_reports(Some(ReportStatus::Open), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.list_reports(Some(ReportStatus::Dismissed), 10)
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        s.list_reports(Some(ReportStatus::Actioned), 10)
            .await
            .unwrap()
            .len(),
        2
    );

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE actor = 'admin' ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        actions,
        vec![
            "report-dismiss",
            "report-dismiss",
            "report-dismiss",
            "deny",
            "report-deny",
            "deny",
            "report-deny",
            "report-dismiss",
        ]
    );
    let detail: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM audit_log WHERE subject = $1 AND action = 'report-dismiss'",
    )
    .bind(format!("report:{r2}"))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(detail["unhidden"], true);
    assert_eq!(detail["reviewed"], true);

    // Free-form audit entries.
    s.audit("admin", "note", "general", &serde_json::json!({"x": 1}))
        .await
        .unwrap();
    assert!(matches!(
        s.audit("admin", "", "general", &serde_json::json!({}))
            .await,
        Err(StoreError::Invalid(_))
    ));
}

#[sqlx::test(migrations = "./migrations")]
async fn lookups_and_stats(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    let a = s.complete(&key(1), &torrent(key(1), "a")).await.unwrap();
    s.complete(&key(2), &torrent(key(2), "b")).await.unwrap();
    let big = s
        .complete(&key(3), &torrent_with_files(key(3), 25))
        .await
        .unwrap();
    s.complete(&key(4), &torrent(key(4), "denied"))
        .await
        .unwrap();
    s.observe(&[obs(key(5), 1), obs(key(6), 1)], NO_LIMIT)
        .await
        .unwrap();

    // get_many returns a preview of the file list; get_by_key all of it.
    let many = s.get_many(&[big, a]).await.unwrap();
    assert_eq!(many.len(), 2);
    assert_eq!(many[0].files.len(), GET_MANY_MAX_FILES);
    assert_eq!(many[0].file_count, 25);
    assert_eq!(
        many[0].files.first().map(|f| f.path.as_str()),
        Some("dir/f000")
    );
    assert_eq!(
        many[0].files.last().map(|f| f.path.clone()),
        Some(format!("dir/f{:03}", GET_MANY_MAX_FILES - 1))
    );
    assert_eq!(many[1].files, torrent(key(1), "a").files);
    let full = s
        .get_by_key(&AnyKey::V1OrDht(key(3)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(full.files.len(), 25);
    assert_eq!(
        TorrentRecord {
            files: full.files[..GET_MANY_MAX_FILES].to_vec(),
            ..full.clone()
        },
        many[0]
    );

    // One hidden, one tombstoned.
    assert!(s.submit_report(&csam(key(1))).await.unwrap().hidden);
    assert!(s.get_many(&[a]).await.unwrap().is_empty());
    s.deny(key(4).as_bytes(), DenyReason::Dmca, None, "admin")
        .await
        .unwrap();

    assert_eq!(
        s.stats().await.unwrap(),
        StoreStats {
            torrents: 2,
            pending: 2,
            gave_up: 0,
            denylisted: 1,
            open_reports: 1
        }
    );
    assert_eq!(
        s.public_stats().await.unwrap(),
        PublicStats {
            torrents: 2,
            added_today: 4,
            added_yesterday: 0
        }
    );

    let days = s.daily_stats(7).await.unwrap();
    assert_eq!(days.len(), 1);
    assert_eq!((days[0].discovered, days[0].fetched), (2, 4));
    assert!(s.daily_stats(0).await.unwrap().is_empty());

    // Older days are included within the window and excluded outside it.
    sqlx::query(
        "INSERT INTO stats_daily (day, fetched) VALUES \
         ((now() AT TIME ZONE 'UTC')::date - 1, 7), \
         ((now() AT TIME ZONE 'UTC')::date - 3, 9), \
         ((now() AT TIME ZONE 'UTC')::date - 10, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let days = s.daily_stats(7).await.unwrap();
    assert_eq!(
        days.iter().map(|d| d.fetched).collect::<Vec<_>>(),
        vec![9, 7, 4]
    );
    assert_eq!(s.daily_stats(u32::MAX).await.unwrap().len(), 4);
    let public = s.public_stats().await.unwrap();
    assert_eq!((public.added_today, public.added_yesterday), (4, 7));

    // After ANALYZE the estimate is small, so the count stays exact.
    sqlx::query("ANALYZE torrents")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(s.stats().await.unwrap().torrents, 2);
    assert_eq!(s.public_stats().await.unwrap().torrents, 2);

    // Above the threshold, the estimate is used.
    let estimate = EXACT_COUNT_THRESHOLD * 3;
    sqlx::query("UPDATE pg_class SET reltuples = $1 WHERE oid = 'torrents'::regclass")
        .bind(estimate as f32)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(s.public_stats().await.unwrap().torrents, estimate);
    assert_eq!(s.stats().await.unwrap().torrents, estimate as u64);
}

#[sqlx::test(migrations = "./migrations")]
async fn settings_are_validated_and_audited(pool: PgPool) {
    let s = Store::from_pool(pool.clone());
    assert_eq!(s.get_setting("missing").await.unwrap(), None);

    assert!(s.set_setting(SETTING_AUTOHIDE_PER_HOUR, "5").await.unwrap());
    assert!(!s.set_setting(SETTING_AUTOHIDE_PER_HOUR, "5").await.unwrap());
    assert_eq!(
        s.get_setting(SETTING_AUTOHIDE_PER_HOUR).await.unwrap(),
        Some("5".into())
    );
    let audit =
        sqlx::query("SELECT actor, subject, detail FROM audit_log WHERE action = 'set-setting'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].get::<String, _>("actor"), "owner");
    assert_eq!(
        audit[0].get::<String, _>("subject"),
        SETTING_AUTOHIDE_PER_HOUR
    );
    assert_eq!(
        audit[0].get::<serde_json::Value, _>("detail"),
        serde_json::json!({"old": "60", "new": "5"})
    );

    for bad in ["", "-1", "abc", " 5", "1234567890"] {
        assert!(
            matches!(
                s.set_setting(SETTING_OPEN_REPORTS_CAP, bad).await,
                Err(StoreError::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    s.set_setting(SETTING_OPEN_REPORTS_CAP, &MAX_NUMERIC_SETTING.to_string())
        .await
        .unwrap();
    assert!(matches!(
        s.set_setting("", "x").await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        s.set_setting(&"k".repeat(MAX_SETTING_KEY_CHARS + 1), "x")
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        s.set_setting("k", &"v".repeat(MAX_SETTING_VALUE_CHARS + 1))
            .await,
        Err(StoreError::Invalid(_))
    ));

    // Other keys are free-form and may be created.
    assert!(s.set_setting("note", "").await.unwrap());
    assert_eq!(s.get_setting("note").await.unwrap(), Some(String::new()));
    let created = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT detail FROM audit_log WHERE action = 'set-setting' AND subject = 'note'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(created, serde_json::json!({"old": null, "new": ""}));

    // The schema enforces the numeric format too.
    let err = sqlx::query("UPDATE settings SET value = '1e3' WHERE key = $1")
        .bind(SETTING_AUTOHIDE_PER_HOUR)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(code_of(&err).as_deref(), Some(SQLSTATE_CHECK_VIOLATION));
}

#[sqlx::test(migrations = "./migrations")]
async fn schema_constraints(pool: PgPool) {
    // Bad key lengths, enum values and file lists are rejected by the database
    // itself.
    let bad = [
        "INSERT INTO pending (dht_key) VALUES ('\\x00')",
        "INSERT INTO denylist (key, reason, created_by) VALUES (decode(repeat('00', 21), 'hex'), 'dmca', 'a')",
        "INSERT INTO denylist (key, reason, created_by) VALUES (decode(repeat('00', 20), 'hex'), 'bogus', 'a')",
        "INSERT INTO reports (dht_key, reason, message) VALUES (decode(repeat('00', 20), 'hex'), 'spam', '')",
        "INSERT INTO reports (dht_key, reason, message, status) VALUES (decode(repeat('00', 20), 'hex'), 'other', '', 'actioned')",
        "INSERT INTO torrents (dht_key, name, total_size, file_count, change_seq, deleted_at) \
         VALUES (decode(repeat('00', 20), 'hex'), 'still named', 0, 0, 1, now())",
        "INSERT INTO torrents (dht_key, name, total_size, file_count, change_seq, info_hash_v2) \
         VALUES (decode(repeat('00', 20), 'hex'), 'x', 0, 0, 1, decode(repeat('00', 20), 'hex'))",
        "INSERT INTO torrents (dht_key, name, total_size, file_count, change_seq, files) \
         VALUES (decode(repeat('00', 20), 'hex'), 'x', 0, 0, 1, '{}')",
        "INSERT INTO torrents (dht_key, name, total_size, file_count, change_seq, files) \
         VALUES (decode(repeat('00', 20), 'hex'), 'x', 0, 0, 1, \
                 (SELECT jsonb_agg(i) FROM generate_series(1, 2001) AS i))",
        "INSERT INTO settings (key, value) VALUES ('open_reports_cap', '-5')",
        "INSERT INTO settings (key, value) VALUES ('', 'x')",
    ];
    for sql in bad {
        let err = sqlx::query(sql).execute(&pool).await.unwrap_err();
        assert_eq!(
            code_of(&err).as_deref(),
            Some(SQLSTATE_CHECK_VIOLATION),
            "{sql}: {err}"
        );
    }

    // The file-list check runs only when files is written.
    let s = Store::from_pool(pool.clone());
    let id = s.complete(&key(1), &torrent(key(1), "t")).await.unwrap();
    sqlx::query("ALTER TABLE torrents DISABLE TRIGGER torrents_files_shape")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE torrents SET files = '{\"not\": \"a list\"}' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE torrents ENABLE TRIGGER torrents_files_shape")
        .execute(&pool)
        .await
        .unwrap();
    s.observe(&[obs(key(1), 1)], NO_LIMIT).await.unwrap();
    let err = sqlx::query("UPDATE torrents SET files = files WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(code_of(&err).as_deref(), Some(SQLSTATE_CHECK_VIOLATION));

    let indexes: HashSet<String> = sqlx::query_scalar(
        "SELECT indexname::text FROM pg_indexes \
          WHERE tablename IN ('torrents', 'pending', 'reports', 'denylist', 'audit_log', 'settings')",
    )
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    for name in [
        "torrents_change_seq",
        "torrents_v2_prefix",
        "torrents_dht_key_key",
        "torrents_info_hash_v1_key",
        "torrents_info_hash_v2_key",
        "pending_ready",
        "pending_gave_up",
        "denylist_prefix",
        "reports_open",
        "reports_torrent",
        "reports_open_csam",
        "audit_log_at",
        "audit_log_action_at",
        "settings_pkey",
    ] {
        assert!(indexes.contains(name), "missing index {name}");
    }
}
