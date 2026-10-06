//! End to end with PostgreSQL (design §13): discovery → pending → fetch →
//! torrents → index → web search → deny.
//!
//! Runs only when `DATABASE_URL` names a superuser connection; the test
//! creates and drops its own database and never touches the one in the URL.
// Helpers outside #[test] functions are not covered by clippy.toml.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{
    NETWORK_NODES, Network, Seeder, announce, crawl_options, http, info_dict, note, seed_policy,
    wait_for_tables, wait_until,
};
use dc3_core::AnyKey;
use dc3_dht::Dht;
use dc3_search::{SearchHandle, SearchQuery};
use dc3_store::sqlx::postgres::PgConnection;
use dc3_store::sqlx::{self, AssertSqlSafe, Connection};
use dc3_store::{DenyReason, Observation, PgConnectOptions, Store, StoreError};
use dc3_web::{WebConfig, WebDeps};
use dhtcrawler3::admin;
use dhtcrawler3::crawl::Crawler;
use dhtcrawler3::fetch::new_torrent;
use dhtcrawler3::index::{self, IndexOptions, READY_MAX_LAG};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The whole scenario must finish within this.
const TEST_LIMIT: Duration = Duration::from_secs(100);
/// Pool size for the test store.
const POOL_SIZE: u32 = 16;
/// The web's public origin in this test.
const BASE_URL: &str = "http://127.0.0.1:8080";

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty())
}

/// A database created for this test, dropped when the test ends (also when
/// it fails).
struct TestDatabase {
    admin: PgConnectOptions,
    name: String,
    dropped: bool,
}

impl TestDatabase {
    async fn create(url: &str) -> TestDatabase {
        let admin: PgConnectOptions = url.parse().expect("DATABASE_URL is not a valid URL");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Only digits and underscores: safe to put in the statement.
        let name = format!("dc3_e2e_{}_{nanos}", std::process::id());
        let mut conn = PgConnection::connect_with(&admin)
            .await
            .expect("cannot connect with DATABASE_URL");
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        TestDatabase {
            admin,
            name,
            dropped: false,
        }
    }

    fn options(&self) -> PgConnectOptions {
        self.admin.clone().database(&self.name)
    }

    async fn drop_now(&mut self) {
        drop_database(&self.admin, &self.name).await;
        self.dropped = true;
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        let (admin, name) = (self.admin.clone(), self.name.clone());
        // Drop may run while a panic unwinds, inside or outside a runtime.
        let _ = std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(drop_database(&admin, &name));
            }
        })
        .join();
    }
}

async fn drop_database(admin: &PgConnectOptions, name: &str) {
    match PgConnection::connect_with(admin).await {
        Ok(mut conn) => {
            let sql = format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)");
            if let Err(e) = sqlx::query(AssertSqlSafe(sql)).execute(&mut conn).await {
                eprintln!("cannot drop test database {name}: {e}");
            }
            let _ = conn.close().await;
        }
        Err(e) => eprintln!("cannot connect to drop test database {name}: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawl_index_search_and_deny() {
    let Some(url) = database_url() else {
        eprintln!("skipping the database end-to-end test: DATABASE_URL is not set");
        return;
    };
    let mut db = TestDatabase::create(&url).await;
    let options = db.options();
    let outcome = tokio::spawn(tokio::time::timeout(TEST_LIMIT, scenario(options))).await;
    db.drop_now().await;
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(_)) => panic!("the database end-to-end test timed out"),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => panic!("the scenario task failed: {e}"),
    }
}

/// Makes failed fetches retry at once instead of after the production
/// backoff, so an unlucky first attempt cannot stall the test.
async fn retry_failed_fetches(store: &Store) {
    sqlx::query(
        "UPDATE pending SET next_attempt_at = now() \
         WHERE NOT gave_up AND next_attempt_at > now() AND lease_until IS NULL",
    )
    .execute(store.pool())
    .await
    .unwrap();
}

async fn search_ids(search: &SearchHandle, text: &str) -> Vec<i64> {
    let refresher = search.clone();
    tokio::task::spawn_blocking(move || refresher.refresh())
        .await
        .unwrap()
        .unwrap();
    search
        .search(SearchQuery::new(text), Duration::from_secs(5))
        .await
        .unwrap()
        .hits
        .iter()
        .map(|h| h.id)
        .collect()
}

async fn scenario(options: PgConnectOptions) {
    let started = Instant::now();
    let store = Store::connect_with(options, POOL_SIZE).await.unwrap();
    let mut out: Vec<u8> = Vec::new();
    admin::migrate(&store, &mut out).await.unwrap();
    let policy = seed_policy();

    // --- Crawl: discovery → pending → fetch → torrents ---
    let network = Network::start(NETWORK_NODES).await;
    let nodes: Vec<&Dht> = network.nodes.iter().collect();
    wait_for_tables(&nodes, 6, Duration::from_secs(20)).await;
    let clean = Seeder::start(info_dict(
        "e2e test torrent",
        &[("a/one.txt", 100), ("b/two.bin", 2_000), ("c.nfo", 30)],
    ))
    .await;
    let blocked = Seeder::start(info_dict("e2e pthc set", &[("x.txt", 1)])).await;
    for seeder in [&clean, &blocked] {
        announce(nodes[1], nodes[2], seeder).await;
    }
    note(started, "torrents announced");

    let cancel = CancellationToken::new();
    let crawler = Crawler::start(
        crawl_options(network.seed()),
        store.clone(),
        Arc::clone(&policy),
        Arc::new(AtomicBool::new(false)),
        cancel.clone(),
    )
    .await
    .unwrap();

    let clean_key = AnyKey::V1OrDht(clean.key);
    wait_until("the torrent to be stored", Duration::from_secs(40), || {
        let store = store.clone();
        async move {
            retry_failed_fetches(&store).await;
            store.get_by_key(&clean_key).await.unwrap().is_some()
        }
    })
    .await;
    wait_until(
        "the blocked torrent to be denied",
        Duration::from_secs(40),
        || {
            let (store, key) = (store.clone(), blocked.key);
            async move {
                retry_failed_fetches(&store).await;
                store.is_denied(&[key.as_bytes()]).await.unwrap()
            }
        },
    )
    .await;
    note(started, "crawled");

    let record = store.get_by_key(&clean_key).await.unwrap().unwrap();
    assert_eq!(record.name, "e2e test torrent");
    assert_eq!(record.file_count, 3);
    assert_eq!(record.files.len(), 3);
    assert_eq!(record.total_size, 2_130);
    assert_eq!(record.info_hash_v1, Some(clean.key));
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.torrents, 1);
    assert_eq!(stats.pending, 0, "both keys left the queue");
    assert_eq!(stats.denylisted, 1);
    let today = store.daily_stats(1).await.unwrap();
    let today = today.last().expect("a stats row for today");
    assert!(today.discovered >= 2, "{today:?}");
    assert!(today.fetched >= 1, "{today:?}");
    assert!(today.blocked >= 1, "{today:?}");
    let denied = store.list_denied(10, 0).await.unwrap();
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].reason, DenyReason::CsamAuto);
    assert_eq!(denied[0].created_by, "crawler");

    // --- Index ---
    let index_dir = tempfile::tempdir().unwrap();
    let index_ready = Arc::new(AtomicBool::new(false));
    let indexer = tokio::spawn(index::run(
        IndexOptions {
            path: index_dir.path().to_path_buf(),
            writer_heap_bytes: dc3_search::WRITER_PROBE_HEAP_BYTES * 2,
            batch_size: 1000,
            poll_interval: Duration::from_millis(100),
            ready_max_lag: READY_MAX_LAG,
        },
        store.clone(),
        Arc::clone(&policy),
        Arc::clone(&index_ready),
        cancel.clone(),
    ));
    let search = dhtcrawler3::web::open_search(index_dir.path())
        .await
        .unwrap();
    let record_id = record.id;
    wait_until("the torrent to be indexed", Duration::from_secs(20), || {
        let search = search.clone();
        async move { search_ids(&search, "e2e").await == vec![record_id] }
    })
    .await;
    wait_until("the indexer to be ready", Duration::from_secs(10), || {
        let ready = Arc::clone(&index_ready);
        async move { ready.load(Ordering::Relaxed) }
    })
    .await;
    note(started, "indexed");

    // --- Web ---
    let web_cfg = WebConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        base_url: BASE_URL.into(),
        site_name: "dhtcrawler3 e2e".into(),
        contact_email: "abuse@example.org".into(),
        dmca_agent: String::new(),
        hsts: false,
        trusted_proxies: Vec::new(),
        seeder_freshness: Duration::from_secs(7 * 24 * 60 * 60),
    };
    let app = dc3_web::router(
        web_cfg,
        WebDeps {
            backend: store.clone(),
            search: search.clone(),
            policy: Arc::clone(&policy),
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let web_addr: SocketAddr = listener.local_addr().unwrap();
    let server = tokio::spawn(
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(cancel.clone().cancelled_owned())
        .into_future(),
    );

    let page = http(web_addr, "GET", "/search?q=e2e", &[], b"").await;
    assert_eq!(page.status, 200, "{page:?}");
    assert!(page.body.contains("e2e test torrent"), "{}", page.body);
    let detail_path = format!("/t/{}", clean.key.to_hex());
    let detail = http(web_addr, "GET", &detail_path, &[], b"").await;
    assert_eq!(detail.status, 200, "{detail:?}");
    assert!(detail.body.contains("e2e test torrent"));
    assert!(detail.body.contains("two.bin"));

    // --- Admin deny: removed for good ---
    let outcome = admin::deny_add(
        &store,
        &clean_key,
        DenyReason::Csam,
        Some("e2e takedown"),
        &mut out,
    )
    .await
    .unwrap();
    assert!(outcome.newly_denied);
    assert_eq!(outcome.tombstoned, 1);
    assert!(store.is_denied(&[clean.key.as_bytes()]).await.unwrap());
    let meta = dc3_torrent::parse_info(&clean.info).unwrap();
    assert!(matches!(
        store
            .complete(&clean.key, &new_torrent(clean.key, meta))
            .await,
        Err(StoreError::Denied)
    ));
    let seen_again = store
        .observe(
            &[Observation {
                key: clean.key,
                sightings: 1,
                priority: true,
            }],
            1_000,
        )
        .await
        .unwrap();
    // The tombstoned row still holds the key, so it counts as known; either
    // way it is never queued again.
    assert_eq!(seen_again.known + seen_again.denied, 1, "{seen_again:?}");
    assert_eq!(seen_again.queued, 0);
    assert_eq!(store.stats().await.unwrap().pending, 0);
    let detail = http(web_addr, "GET", &detail_path, &[], b"").await;
    assert_eq!(detail.status, 404, "{detail:?}");
    // The policy rescan finds nothing new: the blocked torrent was never stored.
    let rescan = admin::policy_rescan(&store, &policy, &mut out)
        .await
        .unwrap();
    assert_eq!(rescan.matched, 0);
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("denied"), "{printed}");
    note(started, "denied");

    // --- Shutdown ---
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(30), crawler.join())
        .await
        .expect("the crawler did not stop")
        .unwrap();
    indexer.await.unwrap().unwrap();
    server.await.unwrap().unwrap();
    drop(nodes);
    network.shutdown().await;
    store.pool().close().await;
    drop(index_dir);
    note(started, "done");
}
