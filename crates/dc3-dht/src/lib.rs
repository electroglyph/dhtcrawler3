//! A Mainline DHT node for dhtcrawler3 (BEP 5, 32, 42, 43, 51).
//!
//! [`Dht::start`] binds one UDP socket per address family and runs:
//! * a compliant responder (`ping`, `find_node`, `get_peers`,
//!   `announce_peer`, `sample_infohashes`) with real tokens, KRPC errors and
//!   its own reply budget;
//! * a routing table per family with BEP 42 node IDs, chosen after an
//!   external-IP vote;
//! * a BEP 51 sampler that emits [`Discovered`] keys and never samples a
//!   node before its `interval` has passed;
//! * iterative lookups for [`Dht::get_peers`] and [`Dht::announce`].
//!
//! Every input is bounded (datagram size, bencode depth and item count, list
//! lengths) and every map and queue has a fixed capacity. Outgoing queries
//! and replies have separate global budgets, and traffic is also limited per
//! address. Every address the node sends to, stores or hands out passes
//! [`is_dialable`]. Discoveries go to a bounded channel; when it is full they
//! are dropped and counted, never buffered.
//!
//! Protocol building blocks are public for tests and tools: [`krpc`]
//! (message codec), [`compact`] (compact encodings, the address chokepoint)
//! and [`node_id`] (XOR metric, BEP 42).
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

pub mod compact;
mod config;
pub mod bloom;
pub mod krpc;
mod lookup;
mod net;
mod node;
pub mod node_id;
mod peer_store;
mod ratelimit;
mod responder;
mod routing;
mod sampler;
mod state;
mod stats;
mod token;
mod util;

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use dc3_core::DhtKey;
use futures::future::join_all;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::DropGuard;

pub use compact::{Family, is_dialable};
pub use config::{
    DEFAULT_BOOTSTRAP, DEFAULT_CLIENT_VERSION, DEFAULT_MAX_PACKETS_PER_SEC,
    DEFAULT_PORT, DEFAULT_RESPONDER_BYTES_PER_SEC, DEFAULT_RESPONDER_REPLIES_PER_SEC,
    DEFAULT_SAMPLER_CONCURRENCY, DEFAULT_SCRAPE_PACKETS_PER_SEC, DhtConfig, DhtTuning,
    MAX_SAMPLER_CONCURRENCY,
};
pub use node::MIN_GOOD_NODES;
pub use node_id::NodeId;
pub use stats::{DhtStatsSnapshot, DropCounts, DropReason, FamilyStats, QueryCounts};

use crate::node::Inner;
use crate::util::{after, lock};

/// Errors from [`Dht::start`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configuration cannot be used.
    #[error("invalid DHT configuration: {0}")]
    Config(String),
    /// A socket could not be bound, and the node cannot run without it.
    #[error("cannot bind DHT socket {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    /// IPv4 is disabled and IPv6 cannot be used: the host has no global
    /// IPv6 address.
    #[error("no DHT socket: IPv4 is disabled and the host has no global IPv6 address")]
    NoSocket,
}

/// How a key was discovered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A BEP 51 `sample_infohashes` answer.
    Sample,
    /// An `announce_peer` query with a valid token.
    Announce,
    /// A `get_peers` query.
    GetPeers,
}

impl Source {
    /// The metric label: `"sample"`, `"announce"` or `"get_peers"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Sample => "sample",
            Source::Announce => "announce",
            Source::GetPeers => "get_peers",
        }
    }
}

/// A DHT key seen by the node. Addresses in it stay in memory only (R11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Discovered {
    pub key: DhtKey,
    pub source: Source,
    /// The announcing peer, for `Source::Announce`. It passed [`is_dialable`].
    pub peer: Option<SocketAddr>,
    /// The source IP of the packet behind the discovery: the querying node
    /// for `Announce` and `GetPeers`, the answering node for `Sample`.
    /// IPv4-mapped addresses are given as IPv4.
    pub from: IpAddr,
}

/// A BEP 33 scrape result for one infohash, merged over the node's
/// address families.
#[derive(Clone, Debug, Default)]
pub struct ScrapeReport {
    /// Distinct dialable peers found (as in [`Dht::get_peers`]).
    pub peers: Vec<std::net::SocketAddr>,
    /// `BFsd` (seed) filters, one per aware response.
    pub seed_filters: Vec<[u8; crate::bloom::BLOOM_LEN]>,
    /// `BFpe` (peer) filters, one per aware response (sanity/dedup only,
    /// never stored).
    pub peer_filters: Vec<[u8; crate::bloom::BLOOM_LEN]>,
    /// Responses that carried filter keys.
    pub aware: usize,
    /// Responses without filter keys.
    pub unaware: usize,
}

impl ScrapeReport {
    /// Estimated seeders: the BEP counting formula over the OR-union of
    /// all aware `BFsd` filters. `None` means UNKNOWN — zero aware
    /// responses, or a saturated union. An empty union (aware responses,
    /// no seeds) estimates to `Some(0)`.
    pub fn seeders_est(&self) -> Option<u64> {
        crate::bloom::estimate_or(&self.seed_filters)
    }
}

/// A running DHT node. Cheap to clone; the node stops when
/// [`shutdown`](Dht::shutdown) is called or the last handle is dropped.
#[derive(Clone)]
pub struct Dht {    inner: Arc<Inner>,
    _stop_on_drop: Arc<DropGuard>,
}

impl std::fmt::Debug for Dht {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dht")
            .field("local_addrs", &self.local_addrs())
            .finish_non_exhaustive()
    }
}

impl Dht {
    /// Binds the sockets and starts the node. Discoveries are sent to `sink`
    /// with `try_send`: when it is full they are dropped and counted.
    pub async fn start(cfg: DhtConfig, sink: mpsc::Sender<Discovered>) -> Result<Dht, Error> {
        let inner = Arc::new(Inner::new(cfg, sink).await?);
        inner.spawn_tasks();
        let guard = Arc::new(inner.cancel.clone().drop_guard());
        Ok(Dht {
            inner,
            _stop_on_drop: guard,
        })
    }

    /// Looks up peers for `key` on every address family (iterative
    /// `get_peers`). Returns the distinct dialable peers found before
    /// `timeout`; IPv4-mapped peers are returned as IPv4. Callers still
    /// reject our own addresses ([`own_addrs`](Dht::own_addrs)).
    pub async fn get_peers(&self, key: DhtKey, timeout: Duration) -> Vec<SocketAddr> {
        let deadline = after(Instant::now(), timeout);
        let lookups = self
            .inner
            .sockets
            .iter()
            .map(|sock| lookup::get_peers(&self.inner, sock, key, deadline));
        let mut seen = HashSet::new();
        join_all(lookups)
            .await
            .into_iter()
            .flat_map(|outcome| outcome.peers)
            .filter(|peer| seen.insert(*peer))
            .collect()
    }

    /// Announces that we are a peer for `key` on `port` (TCP), on every
    /// address family. Returns the number of nodes that accepted it.
    pub async fn announce(&self, key: DhtKey, port: u16) -> usize {
        if port == 0 {
            return 0;
        }
        let deadline = after(Instant::now(), self.inner.cfg.tuning.lookup_timeout);
        let announces = self
            .inner
            .sockets
            .iter()
            .map(|sock| lookup::announce(&self.inner, sock, key, port, deadline));
        join_all(announces)
            .await
            .into_iter()
            .fold(0, usize::saturating_add)
    }

    /// BEP 33 scrape for `key` on every address family: iterative
    /// `get_peers` traversals with `scrape=1` on the dedicated scrape
    /// budget. Filters from all aware responses (both families) are kept
    /// for OR-union by the caller; see [`ScrapeReport::seeders_est`].
    ///
    /// The sibling of [`get_peers`](Dht::get_peers): `get_peers` keeps its
    /// signature so existing callers are untouched.
    pub async fn scrape(&self, key: DhtKey, timeout: Duration) -> ScrapeReport {
        let deadline = after(Instant::now(), timeout);
        let lookups = self
            .inner
            .sockets
            .iter()
            .map(|sock| lookup::scrape(&self.inner, sock, key, deadline));
        let mut peers = Vec::new();
        let mut seen = HashSet::new();
        let mut seed_filters = Vec::new();
        let mut peer_filters = Vec::new();
        let mut aware = 0usize;
        let mut unaware = 0usize;
        for outcome in join_all(lookups).await {
            for peer in outcome.peers {
                if seen.insert(peer) {
                    peers.push(peer);
                }
            }
            seed_filters.extend(outcome.seed_filters);
            peer_filters.extend(outcome.peer_filters);
            aware = aware.saturating_add(outcome.aware);
            unaware = unaware.saturating_add(outcome.unaware);
        }
        ScrapeReport {
            peers,
            seed_filters,
            peer_filters,
            aware,
            unaware,
        }
    }

    /// Current counters and sizes.
    pub fn stats(&self) -> DhtStatsSnapshot {
        self.inner.stats()
    }

    /// The bound socket addresses (with the actual ports).
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.inner.sockets.iter().map(|s| s.local_addr).collect()
    }

    /// Our own IPs: the bound addresses that are not unspecified, plus the
    /// external IPs chosen by vote (or saved in the state file). The
    /// crawler rejects these as peers.
    pub fn own_addrs(&self) -> Vec<IpAddr> {
        self.inner.own().ips.clone()
    }

    /// Good routing-table nodes, summed over the address families. The
    /// crawl role is ready at [`MIN_GOOD_NODES`] or more.
    pub fn good_nodes(&self) -> usize {
        self.inner.good_nodes()
    }

    /// The current node ID of each socket, in the order of [`local_addrs`](Dht::local_addrs).
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.inner.own_ids()
    }

    /// Endpoints currently in the routing tables (diagnostics; memory only).
    pub fn routing_nodes(&self) -> Vec<SocketAddr> {
        self.inner
            .sockets
            .iter()
            .flat_map(|s| {
                lock(&s.state)
                    .table
                    .members()
                    .map(|e| e.addr)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Stops all tasks, waits for them and writes the state file. Other
    /// clones of this handle stop working too.
    pub async fn shutdown(self) {
        self.inner.shutdown().await;
    }
}
