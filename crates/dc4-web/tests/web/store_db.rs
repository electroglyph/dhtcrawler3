//! The web application against a real PostgreSQL database. Runs only when
//! `DATABASE_URL` names a server where the test may create databases; the
//! test creates its own uniquely named database and drops it afterwards.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use dc4_core::{DhtKey, InfoHashV2};
use dc4_search::{IndexDoc, IndexRoot, SearchHandle};
use dc4_store::sqlx::{self, AssertSqlSafe, Connection, PgConnection};
use dc4_store::{FileRow, NewTorrent, PgConnectOptions, Store};
use dc4_web::{Backend, WebDeps, router};

use crate::common::*;

const WRITER_HEAP: usize = 20 * 1024 * 1024;
const POOL_SIZE: u32 = 4;

/// A database created for this test run.
struct TempDatabase {
    server_url: String,
    name: String,
}

impl TempDatabase {
    async fn create(server_url: &str) -> TempDatabase {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Only [a-z0-9_]: safe to place in SQL.
        let name = format!("dc4_web_test_{}_{nanos}", std::process::id());
        let mut conn = PgConnection::connect(server_url).await.unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        TempDatabase {
            server_url: server_url.to_owned(),
            name,
        }
    }

    fn options(&self) -> PgConnectOptions {
        self.server_url
            .parse::<PgConnectOptions>()
            .unwrap()
            .database(&self.name)
    }

    async fn remove(self) {
        let mut conn = PgConnection::connect(&self.server_url).await.unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut conn)
        .await
        .unwrap();
        conn.close().await.unwrap();
    }
}

#[tokio::test]
async fn search_detail_and_readiness_against_postgres() {
    let _serial = serial().await;
    let Some(url) = std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("DATABASE_URL is not set; skipping the PostgreSQL test");
        return;
    };
    metrics();
    let db = TempDatabase::create(&url).await;
    // Run the checks in a task so the database is dropped even if one fails.
    let outcome = tokio::spawn(exercise(db.options())).await;
    db.remove().await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn exercise(options: PgConnectOptions) {
    let store = Store::connect_with(options, POOL_SIZE).await.unwrap();
    store.migrate().await.unwrap();

    let key = DhtKey([0x42; 20]);
    let v2 = InfoHashV2([0x43; 32]);
    store
        .complete(
            &key,
            &NewTorrent {
                dht_key: key,
                info_hash_v1: Some(key),
                info_hash_v2: Some(v2),
                name: "Postgres Integration Übung <b>".into(),
                total_size: 3000,
                file_count: 2,
                files: vec![
                    FileRow {
                        path: "docs/readme.txt".into(),
                        size: 1000,
                    },
                    FileRow {
                        path: "data/blob.bin".into(),
                        size: 2000,
                    },
                ],
                files_truncated: false,
                piece_length: Some(16384),
            },
        )
        .await
        .unwrap();

    // Index the change feed the way the indexer does.
    let mark = store.high_water_mark().await.unwrap().unwrap();
    let rows = store.changes_since(0, mark, 100).await.unwrap();
    assert_eq!(rows.len(), 1);
    let dir = tempfile::tempdir().unwrap();
    let root = IndexRoot::open(dir.path()).unwrap();
    let index = root.open_current().unwrap();
    let mut writer = index.writer(WRITER_HEAP).unwrap();
    for row in &rows {
        assert!(row.visible);
        writer
            .upsert(&IndexDoc {
                id: row.id,
                name: row.name.clone(),
                files: row.files_text.clone(),
                size: row.total_size,
                created: row.first_seen_at.timestamp(),
                seen: row.seen_count,
                file_count: row.file_count,
            })
            .unwrap();
    }
    writer.commit(mark).unwrap();
    writer.wait_merging_threads().unwrap();
    let search = SearchHandle::open(dir.path()).unwrap();

    let app = router(
        config(),
        WebDeps {
            backend: store.clone(),
            search,
        },
    );
    let hex = key.to_hex();

    // Search by name and by file name, as a page and through the API.
    let r = send(&app, get("/search?q=integration")).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.body
            .contains("<bdi>Postgres Integration Übung &#60;b&#62;</bdi>")
    );
    assert!(r.body.contains("1 torrent matches your search."));
    assert_safe_html(&r.body);
    let r = send(&app, get("/search?q=readme")).await;
    assert!(r.body.contains(&format!("href=\"/t/{hex}\"")));
    let r = send(&app, get(&format!("/api/v1/search?q={}", enc("übung")))).await;
    let json = r.json();
    assert_eq!(json["total"], 1);
    assert_eq!(json["results"][0]["dht_key"], hex.as_str());
    assert_eq!(json["results"][0]["info_hash_v2"], v2.to_hex().as_str());
    assert_eq!(json["results"][0]["name"], "Postgres Integration Übung <b>");

    // Details by DHT key and by v2 hash.
    let r = send(&app, get(&format!("/t/{hex}"))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("<bdi>docs/readme.txt</bdi>"));
    assert!(r.body.contains("<bdi>data/blob.bin</bdi>"));
    let r = send(&app, get(&format!("/t/{}", v2.to_hex()))).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = send(&app, get(&format!("/api/v1/torrents/{hex}"))).await;
    assert_eq!(r.json()["files"].as_array().unwrap().len(), 2);

    // Readiness and statistics.
    assert_eq!(send(&app, get("/readyz")).await.status, StatusCode::OK);
    let stats = Backend::public_stats(&store).await.unwrap();
    assert_eq!(stats.torrents, 1);
    assert_eq!(stats.added_today, 1);

    store.pool().close().await;
}
