//! Local preview server: the real site router with an in-memory backend
//! and a temporary search index, so the pages (including the theme switch)
//! can be viewed without Postgres or DHT networking.
//!
//! Run with `cargo run -p dc3-web --example preview`, then open
//! <http://127.0.0.1:8130>. Stop with Ctrl-C.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use dc3_core::{AnyKey, DhtKey};
use dc3_search::{IndexDoc, IndexRoot, SearchHandle};
use dc3_store::{FileRow, PublicStats, TorrentRecord};
use dc3_web::{Backend, BackendError, WebConfig, WebDeps, serve};

#[derive(Clone, Default)]
struct PreviewBackend(Arc<Mutex<Vec<TorrentRecord>>>);

fn key_for(id: i64) -> DhtKey {
    let mut bytes = [0u8; 20];
    bytes[..8].copy_from_slice(&id.to_be_bytes());
    bytes[19] = 0x5a;
    DhtKey(bytes)
}

fn sample(id: i64, name: &str, files: &[(&str, u64)]) -> TorrentRecord {
    let key = key_for(id);
    TorrentRecord {
        id,
        dht_key: key,
        info_hash_v1: Some(key),
        info_hash_v2: None,
        name: name.to_owned(),
        total_size: files.iter().map(|(_, s)| s).sum(),
        file_count: files.len() as u64,
        files: files
            .iter()
            .map(|(p, s)| FileRow {
                path: (*p).to_owned(),
                size: *s,
            })
            .collect(),
        files_truncated: false,
        piece_length: Some(16384),
        seen_count: 7,
        first_seen_at: Utc.with_ymd_and_hms(2026, 9, 1, 12, 30, 0).unwrap(),
        last_seen_at: Utc.with_ymd_and_hms(2026, 9, 15, 12, 30, 0).unwrap(),
        last_scraped_at: None,
        seeders_est: None,
        change_seq: 99,
        deleted_at: None,
    }
}

impl Backend for PreviewBackend {
    async fn get_by_key(&self, key: AnyKey) -> Result<Option<TorrentRecord>, BackendError> {
        let torrents = self.0.lock().unwrap();
        let found = torrents.iter().find(|t| match key {
            AnyKey::V1OrDht(k) => t.dht_key == k || t.info_hash_v1 == Some(k),
            AnyKey::V2(h) => t.info_hash_v2 == Some(h),
        });
        Ok(found.filter(|t| t.is_live()).cloned())
    }

    async fn get_many(&self, ids: &[i64]) -> Result<Vec<TorrentRecord>, BackendError> {
        let torrents = self.0.lock().unwrap();
        Ok(ids
            .iter()
            .filter_map(|id| torrents.iter().find(|t| t.id == *id && t.is_live()))
            .cloned()
            .collect())
    }

    async fn public_stats(&self) -> Result<PublicStats, BackendError> {
        let torrents = self.0.lock().unwrap();
        Ok(PublicStats {
            torrents: torrents.len() as i64,
            added_today: 1,
            added_yesterday: 2,
        })
    }

    async fn ping(&self) -> Result<(), BackendError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), dc3_web::WebError> {
    let torrents = vec![
        sample(
            1,
            "Ubuntu 24.04 LTS Desktop (preview)",
            &[("ubuntu-24.04.iso", 5_872_353_280)],
        ),
        sample(
            2,
            "Debian 12 Bookworm netinst (preview)",
            &[("debian-12-netinst.iso", 659_554_112)],
        ),
        sample(
            3,
            "Sample Project Files (preview)",
            &[("readme.txt", 2048), ("src/main.rs", 8192)],
        ),
    ];

    let dir = tempfile::tempdir().expect("temporary index directory");
    let root = IndexRoot::open(dir.path()).expect("index root");
    let index = root.open_current().expect("current index");
    let mut writer = index.writer(20 * 1024 * 1024).expect("index writer");
    for t in &torrents {
        let files: Vec<&str> = t.files.iter().map(|f| f.path.as_str()).collect();
        writer
            .upsert(&IndexDoc {
                id: t.id,
                name: t.name.clone(),
                files: files.join("\n"),
                size: t.total_size,
                created: t.first_seen_at.timestamp(),
                seen: t.seen_count,
                file_count: t.file_count,
            })
            .expect("index sample torrent");
    }
    writer.commit(1).expect("commit sample index");
    writer.wait_merging_threads().expect("merge sample index");
    let search = SearchHandle::open(dir.path()).expect("search handle");

    let cfg = WebConfig {
        listen: "127.0.0.1:8130".parse().unwrap(),
        base_url: "http://127.0.0.1:8130".into(),
        site_name: "dhtcrawler4 (preview)".into(),
        hsts: false,
        trusted_proxies: Vec::new(),
        seeder_freshness: Duration::from_secs(7 * 24 * 60 * 60),
        search_cache_entries: dc3_web::DEFAULT_SEARCH_CACHE_ENTRIES,
        search_cache_ttl: Duration::from_secs(dc3_web::DEFAULT_SEARCH_CACHE_TTL_SECS),
    };
    let deps = WebDeps {
        backend: PreviewBackend(Arc::new(Mutex::new(torrents))),
        search,
    };
    println!("preview at http://127.0.0.1:8130 (Ctrl-C to stop)");
    // Keeps the temporary index alive for the life of the server.
    let _dir = dir;
    serve(cfg, deps, std::future::pending()).await
}
