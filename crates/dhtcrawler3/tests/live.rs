//! Live-network tests against the public BitTorrent DHT.
//!
//! These are opt-in and never run in CI:
//!
//! ```sh
//! DHTCRAWLER_LIVE=1 cargo test -p dhtcrawler3 --test live -- --ignored --test-threads=1
//! ```
//!
//! Rules the suite enforces on itself:
//!
//! - Every test is `#[ignore]`d and additionally requires `DHTCRAWLER_LIVE=1`,
//!   so a plain `cargo test` never sends a packet off-host.
//! - Tests run serially (`--test-threads=1`) and each bootstraps its own
//!   node, so one failure never cascades into the next test.
//! - The suite is read-only: it never calls `announce`, runs the sampler at
//!   the node's own production budgets, fetches from at most 4 concurrent
//!   peers, and every test has an outer timeout.
//! - No infohash is pinned: fixtures are sampled live from the network
//!   (`sample_infohashes`), so the suite cannot rot and never targets a
//!   specific torrent. Assertions check protocol behaviour, structure and
//!   hash integrity, never content.
// Helpers outside #[test] functions are not covered by clippy.toml's test
// exemptions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

use common::{note, wait_until};
use dc3_core::DhtKey;
use dc3_dht::{DEFAULT_BOOTSTRAP, Dht, DhtConfig, Discovered, MIN_GOOD_NODES, Source};
use dc3_peer::{FetchLimits, fetch_metadata};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Live tests only run with this set: a plain `cargo test` (and CI) must
/// never touch the public network.
fn require_live_env() {
    assert!(
        std::env::var_os("DHTCRAWLER_LIVE").is_some_and(|v| v == "1"),
        "live tests need DHTCRAWLER_LIVE=1; refusing to touch the public DHT"
    );
}

/// A production-shaped node on ephemeral ports: default bootstrap routers,
/// default budgets, default (production) tuning. Only the sampler is
/// switched per test.
fn live_config(sampler: bool) -> DhtConfig {
    let mut cfg = DhtConfig::default();
    cfg.bind_v4 = Some("0.0.0.0:0".parse().unwrap());
    cfg.bind_v6 = Some("[::]:0".parse().unwrap());
    cfg.bootstrap = DEFAULT_BOOTSTRAP.iter().map(|s| (*s).to_owned()).collect();
    cfg.state_file = None;
    cfg.sampler = sampler;
    cfg
}

/// A running live node plus its discovery channel.
struct LiveNode {
    dht: Dht,
    rx: mpsc::Receiver<Discovered>,
    started: Instant,
}

impl LiveNode {
    async fn start(sampler: bool) -> LiveNode {
        let (tx, rx) = mpsc::channel(4096);
        let dht = Dht::start(live_config(sampler), tx)
            .await
            .expect("bind sockets for the live node");
        LiveNode {
            dht,
            rx,
            started: Instant::now(),
        }
    }

    /// Waits for a ready routing table: the same threshold the crawl role
    /// uses. Tolerates dead bootstrap routers (2 of 4 were silent when the
    /// suite was written) since the node tries them all.
    async fn wait_for_table(&self) {
        wait_until(
            "the live routing table to fill",
            Duration::from_secs(6 * 60),
            || {
                let dht = self.dht.clone();
                async move { dht.good_nodes() >= MIN_GOOD_NODES }
            },
        )
        .await;
        note(
            self.started,
            &format!("routing table ready: {} good nodes", self.dht.good_nodes()),
        );
    }

    async fn shutdown(self) {
        self.dht.shutdown().await;
    }
}

/// Test 1: bootstrapping the public DHT fills the routing table and learns
/// our external address through other nodes' responses.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_bootstrap_fills_routing_table() {
    require_live_env();
    tokio::time::timeout(Duration::from_secs(8 * 60), async {
        let node = LiveNode::start(false).await;
        node.wait_for_table().await;

        // External-IP voting: routers see our source address, so our own
        // addresses must gain a non-loopback entry.
        wait_until(
            "external-IP voting to report our address",
            Duration::from_secs(60),
            || {
                let dht = node.dht.clone();
                async move {
                    dht.own_addrs()
                        .iter()
                        .any(|ip| !ip.is_loopback() && !ip.is_unspecified() && !is_unique_local(ip))
                }
            },
        )
        .await;

        let stats = node.dht.stats();
        assert!(
            stats.packets_out() > 0 && stats.responses_received > 0,
            "no two-way traffic with the live network: {stats:?}"
        );
        note(
            node.started,
            &format!("external addrs: {:?}", node.dht.own_addrs()),
        );
        node.shutdown().await;
    })
    .await
    .expect("the live bootstrap test timed out");
}

/// Test 2: iterative lookups of random targets actually traverse the live
/// network — `find_node` queries go out and responses come back — instead
/// of stalling or short-circuiting locally.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_lookup_traverses_the_network() {
    require_live_env();
    tokio::time::timeout(Duration::from_secs(10 * 60), async {
        let node = LiveNode::start(false).await;
        node.wait_for_table().await;

        let before = node.dht.stats();
        for round in 0..5 {
            let target = DhtKey(rand::random::<[u8; 20]>());
            // The timeout bounds the call; the assertions below prove the
            // lookup did real iterative work rather than giving up.
            let _ = node.dht.get_peers(target, Duration::from_secs(45)).await;
            note(node.started, &format!("random lookup {round} done"));
        }
        let after = node.dht.stats();
        assert!(
            after.queries_sent.find_node > before.queries_sent.find_node,
            "lookups sent no find_node queries: {after:?}"
        );
        assert!(
            after.responses_received > before.responses_received,
            "lookups got no responses: {after:?}"
        );
        node.shutdown().await;
    })
    .await
    .expect("the live lookup test timed out");
}

/// Collects up to `want` distinct BEP 51 sample keys, ignoring announces
/// and get_peers discoveries from the same channel.
async fn collect_samples(
    rx: &mut mpsc::Receiver<Discovered>,
    started: Instant,
    want: usize,
    limit: Duration,
) -> Vec<DhtKey> {
    let deadline = Instant::now() + limit;
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    while keys.len() < want && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(d)) if d.source == Source::Sample && seen.insert(d.key) => {
                keys.push(d.key);
                if keys.len() % 5 == 0 {
                    note(started, &format!("{} live samples", keys.len()));
                }
            }
            // Closed sink, lagged receiver, or deadline elapsed: stop.
            _ => break,
        }
    }
    keys
}

/// Test 3: keys sampled live from the network resolve through `get_peers`.
/// Every response is protocol-valid by construction; the test passes when at
/// least one sampled key still has live peers (churn kills most).
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_get_peers_finds_real_peers() {
    require_live_env();
    tokio::time::timeout(Duration::from_secs(14 * 60), async {
        let mut node = LiveNode::start(true).await;
        node.wait_for_table().await;

        let keys =
            collect_samples(&mut node.rx, node.started, 10, Duration::from_secs(8 * 60)).await;
        assert!(
            !keys.is_empty(),
            "the live sampler produced no keys in 8 minutes"
        );

        let mut with_peers = 0;
        for key in &keys {
            let peers = node.dht.get_peers(*key, Duration::from_secs(30)).await;
            if !peers.is_empty() {
                with_peers += 1;
                note(node.started, &format!("{key}: {} peers", peers.len()));
            }
        }
        assert!(
            with_peers >= 1,
            "none of {} sampled keys had live peers",
            keys.len()
        );
        node.shutdown().await;
    })
    .await
    .expect("the live get_peers test timed out");
}

/// Limits for fetching from strangers: short enough that dead peers do not
/// eat the test, long enough for a slow real seeder.
fn live_fetch_limits() -> FetchLimits {
    FetchLimits {
        connect: Duration::from_secs(5),
        handshake: Duration::from_secs(5),
        total: Duration::from_secs(30),
        max_metadata: 8 * 1024 * 1024,
        byte_budget: None,
    }
}

/// Fetches metadata for sampled keys, trying peers in `get_peers` order
/// until `want` succeed or `max_attempts` peers are exhausted. Returns the
/// verified (key, info) pairs plus the number of peers tried.
async fn fetch_from_live_peers(
    node: &LiveNode,
    keys: &[DhtKey],
    want: usize,
    max_attempts: usize,
) -> (Vec<(DhtKey, Vec<u8>)>, usize) {
    let own: HashSet<IpAddr> = node.dht.own_addrs().into_iter().collect();
    let mut tried = HashSet::new();
    let mut fetched = Vec::new();
    let limits = live_fetch_limits();
    'keys: for key in keys {
        let peers = node.dht.get_peers(*key, Duration::from_secs(30)).await;
        for peer in peers {
            if !tried.insert(peer) || own.contains(&peer.ip()) {
                continue;
            }
            match fetch_metadata(peer, *key, &limits).await {
                Ok(info) => {
                    note(node.started, &format!("fetched {key} from {peer}"));
                    fetched.push((*key, info));
                    if fetched.len() >= want {
                        break 'keys;
                    }
                }
                Err(e) => {
                    note(node.started, &format!("{peer} for {key}: {e:?}"));
                }
            }
            if tried.len() >= max_attempts {
                break 'keys;
            }
        }
    }
    (fetched, tried.len())
}

/// Test 4: metadata fetched from real peers is authentic and parses —
/// `fetch_metadata` verifies the hash, and the test additionally pins the
/// SHA-1 itself, parses with `dc3-torrent`, and checks the v1 `pieces`
/// invariant on the raw dictionary.
#[ignore]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_fetch_verifies_real_metadata() {
    require_live_env();
    tokio::time::timeout(Duration::from_secs(20 * 60), async {
        let mut node = LiveNode::start(true).await;
        node.wait_for_table().await;

        let keys =
            collect_samples(&mut node.rx, node.started, 10, Duration::from_secs(8 * 60)).await;
        assert!(
            !keys.is_empty(),
            "the live sampler produced no keys in 8 minutes"
        );
        let (fetched, attempts) = fetch_from_live_peers(&node, &keys, 1, 12).await;
        assert!(
            !fetched.is_empty(),
            "no peer yielded metadata after {attempts} attempts"
        );

        for (key, info) in &fetched {
            // Belt and braces: the fetcher verifies this, but the test owns
            // the live-network authenticity claim.
            use sha1::{Digest, Sha1};
            assert_eq!(
                &Sha1::digest(info)[..],
                &key.as_bytes()[..],
                "fetched bytes do not hash to the requested key"
            );
            let meta = dc3_torrent::parse_info(info)
                .unwrap_or_else(|e| panic!("live metadata for {key} must parse: {e:?}"));
            let root = dc3_bencode::decode(info, &dc3_bencode::Limits::METADATA)
                .expect("live metadata is bencode");
            if let Some(pieces) = root.as_dict().and_then(|d| d.get_bytes(b"pieces")) {
                assert_eq!(
                    pieces.len() % 20,
                    0,
                    "v1 pieces for {key} is not a multiple of 20"
                );
            }
            note(
                node.started,
                &format!("{}: {:?} files", meta.name, meta.file_count),
            );
        }
        node.shutdown().await;
    })
    .await
    .expect("the live fetch test timed out");
}

/// ULA (`fc00::/7`) addresses are never a usable external identity.
fn is_unique_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(_) => false,
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}
