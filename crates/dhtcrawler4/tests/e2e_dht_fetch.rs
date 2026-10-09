//! End to end without a database or the internet (design §13): a private
//! DHT of 16 nodes on 127.0.0.1, test seeders, and the crawler's
//! discovery → admission → fetch → verify → parse path writing to
//! an in-memory store.
// Helpers outside #[test] functions are not covered by clippy.toml.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{
    NETWORK_NODES, Network, Seeder, announce, crawl_options, info_dict, note, wait_for_tables,
    wait_until,
};
use dc3_dht::Dht;
use dhtcrawler4::crawl::Crawler;
use dhtcrawler4::memstore::MemoryStore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The whole scenario must finish within this.
const TEST_LIMIT: Duration = Duration::from_secs(90);
/// Time allowed from the crawler's start to the stored results.
const CRAWL_LIMIT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crawler_discovers_and_fetches() {
    tokio::time::timeout(TEST_LIMIT, scenario())
        .await
        .expect("the end-to-end test timed out");
}

async fn scenario() {
    let started = Instant::now();
    let network = Network::start(NETWORK_NODES).await;
    let nodes: Vec<&Dht> = network.nodes.iter().collect();
    wait_for_tables(&nodes, 6, Duration::from_secs(20)).await;
    note(started, "network ready");

    let clean = Seeder::start(info_dict(
        "e2e test torrent",
        &[
            ("docs/readme.txt", 1_200),
            ("video/e2e sample.mkv", 700_000),
            ("e2e.nfo", 300),
        ],
    ))
    .await;
    let second = Seeder::start(info_dict(
        "e2e second collection",
        &[("a.txt", 10), ("b.txt", 20)],
    ))
    .await;
    let third = Seeder::start(info_dict(
        "e2e holiday photos",
        &[("day1/beach.jpg", 10), ("day2/sunset/x.jpg", 20)],
    ))
    .await;
    // Announced before the crawler joins, so the crawler can only learn the
    // keys through BEP 51 samples.
    for seeder in [&clean, &second, &third] {
        announce(nodes[1], nodes[2], seeder).await;
    }
    note(started, "torrents announced");

    let store = MemoryStore::with_fail_backoff(Duration::from_millis(500));
    let ready = Arc::new(AtomicBool::new(false));
    let cancel = CancellationToken::new();
    let crawler = Crawler::start(
        crawl_options(network.seed()),
        store.clone(),
        Arc::clone(&ready),
        cancel.clone(),
    )
    .await
    .unwrap();
    let joined = Instant::now();

    wait_until("the crawler to store the torrents", CRAWL_LIMIT, || {
        let store = store.clone();
        let keys = [clean.key, second.key, third.key];
        async move { keys.iter().all(|key| store.torrent(key).is_some()) }
    })
    .await;
    note(
        started,
        &format!("crawled {:?} after the crawler started", joined.elapsed()),
    );

    // The clean torrent was fetched from the seeder, verified and parsed.
    let stored = store.torrent(&clean.key).unwrap();
    assert_eq!(stored.dht_key, clean.key);
    assert_eq!(stored.info_hash_v1, Some(clean.key));
    assert_eq!(stored.info_hash_v2, None);
    assert_eq!(stored.name, "e2e test torrent");
    assert_eq!(stored.file_count, 3);
    assert_eq!(stored.total_size, 701_500);
    let paths: Vec<&str> = stored.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        ["docs/readme.txt", "e2e.nfo", "video/e2e sample.mkv"]
    );
    assert!(!stored.files_truncated);
    assert_eq!(stored.piece_length, Some(262_144));

    // Every announced torrent was fetched from its seeder, verified and
    // parsed.
    for seeder in [&clean, &second, &third] {
        assert!(
            store.torrent(&seeder.key).is_some(),
            "missing torrent for {}",
            seeder.key.to_hex()
        );
    }
    assert_eq!(store.torrent_count(), 3);
    assert!(store.pending_keys().is_empty());

    // The keys reached the queue as priority keys, found by BEP 51.
    let observed = store.observed();
    for key in [clean.key, second.key, third.key] {
        assert!(
            observed.iter().any(|o| o.key == key && o.priority),
            "{key} was not observed as a priority key"
        );
    }
    let stats = crawler.dht().stats();
    assert!(stats.queries_sent.sample_infohashes > 0);
    assert!(stats.v4.samples >= 3, "{stats:?}");
    assert_eq!(stats.sampler_early, 0);

    wait_until("the crawler to be ready", Duration::from_secs(20), || {
        let ready = Arc::clone(&ready);
        async move { ready.load(Ordering::Relaxed) }
    })
    .await;

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(30), crawler.join())
        .await
        .expect("the crawler did not stop")
        .unwrap();
    assert!(!ready.load(Ordering::Relaxed));
    drop(nodes);
    network.shutdown().await;
    note(started, "done");
}
