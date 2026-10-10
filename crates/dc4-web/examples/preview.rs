//! Local preview server: the real site router with an in-memory backend
//! and a temporary search index, so the pages (including the theme switch)
//! can be viewed without Postgres or DHT networking.
//!
//! Run with `cargo run -p dc4-web --example preview`, then open
//! <http://127.0.0.1:8130>. Stop with Ctrl-C.

use std::io::{Error as IoError, ErrorKind};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use dc4_core::{AnyKey, DhtKey};
use dc4_search::{IndexDoc, IndexRoot, SearchHandle};
use dc4_store::{FileRow, PublicStats, TorrentRecord};
use dc4_web::{Backend, BackendError, WebConfig, WebDeps, serve};

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Clone, Default)]
struct PreviewBackend(Arc<Mutex<Vec<TorrentRecord>>>);

fn key_for(id: i64) -> DhtKey {
    let mut bytes = [0u8; 20];
    bytes[..8].copy_from_slice(&id.to_be_bytes());
    bytes[19] = 0x5a;
    DhtKey(bytes)
}

fn preview_time(year: i32, month: u32, day: u32) -> Result<DateTime<Utc>, Error> {
    Utc.with_ymd_and_hms(year, month, day, 12, 30, 0)
        .single()
        .ok_or_else(|| {
            IoError::new(
                ErrorKind::InvalidInput,
                format!("bad preview date {year}-{month:02}-{day:02}"),
            )
            .into()
        })
}

fn sample(id: i64, name: &str, files: &[(&str, u64)]) -> Result<TorrentRecord, Error> {
    let key = key_for(id);
    Ok(TorrentRecord {
        id,
        dht_key: key,
        info_hash_v1: Some(key),
        info_hash_v2: None,
        name: name.to_owned(),
        total_size: files.iter().map(|(_, s)| s).sum(),
        file_count: u64::try_from(files.len())
            .map_err(|_| IoError::new(ErrorKind::InvalidInput, "too many preview files"))?,
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
        first_seen_at: preview_time(2026, 9, 1)?,
        last_seen_at: preview_time(2026, 9, 15)?,
        last_scraped_at: None,
        seeders_est: None,
        change_seq: 99,
        deleted_at: None,
    })
}

impl Backend for PreviewBackend {
    async fn get_by_key(&self, key: AnyKey) -> Result<Option<TorrentRecord>, BackendError> {
        let torrents = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let found = torrents.iter().find(|t| match key {
            AnyKey::V1OrDht(k) => t.dht_key == k || t.info_hash_v1 == Some(k),
            AnyKey::V2(h) => t.info_hash_v2 == Some(h),
        });
        Ok(found.filter(|t| t.is_live()).cloned())
    }

    async fn get_many(&self, ids: &[i64]) -> Result<Vec<TorrentRecord>, BackendError> {
        let torrents = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(ids
            .iter()
            .filter_map(|id| torrents.iter().find(|t| t.id == *id && t.is_live()))
            .cloned()
            .collect())
    }

    async fn public_stats(&self) -> Result<PublicStats, BackendError> {
        let torrents = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let total = i64::try_from(torrents.len())
            .map_err(|_| BackendError::Unavailable("too many preview torrents".into()))?;
        Ok(PublicStats {
            torrents: total,
            added_today: 1,
            added_yesterday: 2,
        })
    }

    async fn ping(&self) -> Result<(), BackendError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("preview: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Error> {
    let torrents = vec![
        sample(
            1,
            "Ubuntu 24.04 LTS Desktop (preview)",
            &[("ubuntu-24.04.iso", 5_872_353_280)],
        )?,
        sample(
            2,
            "Debian 12 Bookworm netinst (preview)",
            &[("debian-12-netinst.iso", 659_554_112)],
        )?,
        sample(
            3,
            "Sample Project Files (preview)",
            &[("readme.txt", 2048), ("src/main.rs", 8192)],
        )?,
    ];

    let dir = tempfile::tempdir()?;
    let root = IndexRoot::open(dir.path())?;
    let index = root.open_current()?;
    let mut writer = index.writer(20 * 1024 * 1024)?;
    for t in &torrents {
        let files: Vec<&str> = t.files.iter().map(|f| f.path.as_str()).collect();
        writer.upsert(&IndexDoc {
            id: t.id,
            name: t.name.clone(),
            files: files.join("\n"),
            size: t.total_size,
            created: t.first_seen_at.timestamp(),
            seen: t.seen_count,
            file_count: t.file_count,
        })?;
    }
    writer.commit(1)?;
    writer.wait_merging_threads()?;
    let search = SearchHandle::open(dir.path())?;

    let cfg = WebConfig {
        listen: "127.0.0.1:8130".parse()?,
        site_name: "dhtcrawler4 (preview)".into(),
        hsts: false,
        trusted_proxies: Vec::new(),
        seeder_freshness: Duration::from_secs(7 * 24 * 60 * 60),
        search_cache_entries: dc4_web::DEFAULT_SEARCH_CACHE_ENTRIES,
        search_cache_ttl: Duration::from_secs(dc4_web::DEFAULT_SEARCH_CACHE_TTL_SECS),
    };
    let deps = WebDeps {
        backend: PreviewBackend(Arc::new(Mutex::new(torrents))),
        search,
    };
    println!("preview at http://127.0.0.1:8130 (Ctrl-C to stop)");
    // Keeps the temporary index alive for the life of the server.
    let _dir = dir;
    serve(cfg, deps, std::future::pending())
        .await
        .map_err(Error::from)
}
