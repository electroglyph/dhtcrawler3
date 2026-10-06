//! Iterative Kademlia lookups: `find_node`, `get_peers` and `announce`.
//!
//! A lookup queries the closest known nodes in rounds of α = 3. It stops when
//! a round that got answers finds nothing closer (after one final sweep over
//! the K closest nodes not yet asked), after 8 rounds, or at its deadline.
//! A query that has not answered within `query_slow_after` no longer holds
//! up its round, but its reply is still used if it arrives in time.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use dc3_core::DhtKey;
use futures::StreamExt;
use futures::future::join_all;
use futures::stream::FuturesUnordered;
use tokio::time::Instant;

use crate::compact::{AddrPolicy, CompactNode, Family, OwnAddrs, canonical_addr};
use crate::krpc::{Method, Response};
use crate::node::{Inner, QueryError, SocketNode};
use crate::node_id::{Distance, NodeId};
use crate::routing::K;
use crate::util::{after, lock};

/// Queries per round.
pub(crate) const ALPHA: usize = 3;
/// Rounds per lookup.
pub(crate) const MAX_ROUNDS: usize = 8;
/// Candidates remembered per lookup.
const MAX_CANDIDATES: usize = 128;
/// Nodes taken from one response.
const MAX_NODES_PER_RESPONSE: usize = 16;
/// Peers collected per lookup.
pub(crate) const MAX_LOOKUP_PEERS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    FindNode,
    GetPeers,
    /// BEP 33 scrape: a `get_peers` traversal with `scrape=1` that also
    /// collects `BFsd`/`BFpe` filters from aware responses.
    Scrape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CandState {
    Fresh,
    InFlight,
    Responded,
    Failed,
}

struct Candidate {
    node: CompactNode,
    dist: Distance,
    state: CandState,
    token: Option<Vec<u8>>,
}

/// What a lookup found.
pub(crate) struct Outcome {
    /// Nodes that answered, closest first (at most K), with their tokens.
    pub(crate) closest: Vec<(CompactNode, Option<Vec<u8>>)>,
    /// Dialable peers from `values`, IPv4-mapped ones as IPv4 (get_peers only).
    pub(crate) peers: Vec<SocketAddr>,
}

/// What a scrape traversal found: like [`Outcome`] plus the BEP 33
/// filters of every aware response (one entry per response that carried
/// filter keys) and the aware/unaware response counts.
pub(crate) struct ScrapeOutcome {
    /// Dialable peers from `values`, IPv4-mapped ones as IPv4.
    pub(crate) peers: Vec<SocketAddr>,
    /// `BFsd` filters of aware responses (seeds).
    pub(crate) seed_filters: Vec<[u8; crate::bloom::BLOOM_LEN]>,
    /// `BFpe` filters of aware responses (peers).
    pub(crate) peer_filters: Vec<[u8; crate::bloom::BLOOM_LEN]>,
    /// Responses that carried filter keys.
    pub(crate) aware: usize,
    /// Responses without filter keys.
    pub(crate) unaware: usize,
}

impl ScrapeOutcome {
    /// OR-union of all `BFsd` filters; `None` when no response was aware
    /// (UNKNOWN, §3) or the union is saturated (UNKNOWN, §0).
    pub fn seeders_union(&self) -> Option<crate::bloom::ScrapeBloom> {
        if self.aware == 0 {
            return None;
        }
        let mut union = crate::bloom::ScrapeBloom::empty();
        for f in &self.seed_filters {
            union.union_into(&crate::bloom::ScrapeBloom(*f));
        }
        Some(union)
    }

    /// Estimated seeders from the OR-union of all aware `BFsd` filters:
    /// `None` means UNKNOWN (no aware response, or a saturated union).
    /// An empty union (live entries but no seeds seen) estimates to `0`.
    pub fn seeders_est(&self) -> Option<u64> {
        crate::bloom::estimate_or(&self.seed_filters)
    }
}

/// The pure bookkeeping of one lookup.
struct Lookup {
    target: NodeId,
    kind: Kind,
    family: Family,
    policy: AddrPolicy,
    own_ids: Vec<NodeId>,
    own: Arc<OwnAddrs>,
    /// Sorted by distance to `target`.
    cands: Vec<Candidate>,
    seen_addrs: HashSet<SocketAddr>,
    seen_ids: HashSet<NodeId>,
    peers: Vec<SocketAddr>,
    peer_set: HashSet<SocketAddr>,
    /// `BFsd` filters of aware scrape responses (one per response).
    seed_filters: Vec<[u8; crate::bloom::BLOOM_LEN]>,
    /// `BFpe` filters of aware scrape responses (one per response).
    peer_filters: Vec<[u8; crate::bloom::BLOOM_LEN]>,
    /// Scrape responses that carried filter keys.
    aware: usize,
    /// Scrape responses without filter keys.
    unaware: usize,
}

impl Lookup {
    fn new(
        target: NodeId,
        kind: Kind,
        family: Family,
        policy: AddrPolicy,
        own: Arc<OwnAddrs>,
    ) -> Self {
        Self {
            target,
            kind,
            family,
            policy,
            own_ids: Vec::new(),
            own,
            cands: Vec::new(),
            seen_addrs: HashSet::new(),
            seen_ids: HashSet::new(),
            peers: Vec::new(),
            peer_set: HashSet::new(),
            seed_filters: Vec::new(),
            peer_filters: Vec::new(),
            aware: 0,
            unaware: 0,
        }
    }

    /// Adds a candidate; returns its distance if it was new. Candidates of
    /// another family (including IPv4-mapped ones), non-dialable addresses
    /// and our own IDs and addresses are ignored.
    fn add(&mut self, node: CompactNode) -> Option<Distance> {
        if self.own_ids.contains(&node.id)
            || Family::of(&node.addr) != self.family
            || canonical_addr(node.addr) != node.addr
            || !self.policy.dialable(&node.addr)
            || self.own.contains(&node.addr, self.policy)
            || self.seen_addrs.contains(&node.addr)
            || self.seen_ids.contains(&node.id)
        {
            return None;
        }
        let dist = node.id.distance(&self.target);
        let pos = self.cands.partition_point(|c| c.dist < dist);
        if pos >= MAX_CANDIDATES {
            return None;
        }
        self.seen_addrs.insert(node.addr);
        self.seen_ids.insert(node.id);
        self.cands.insert(
            pos,
            Candidate {
                node,
                dist,
                state: CandState::Fresh,
                token: None,
            },
        );
        if self.cands.len() > MAX_CANDIDATES
            && let Some(i) = self
                .cands
                .iter()
                .rposition(|c| matches!(c.state, CandState::Fresh | CandState::Failed))
        {
            self.cands.remove(i);
        }
        Some(dist)
    }

    fn best_live(&self) -> Option<Distance> {
        self.cands
            .iter()
            .find(|c| c.state != CandState::Failed)
            .map(|c| c.dist)
    }

    /// Up to `n` unasked nodes among the K closest live candidates, marked in flight.
    fn pick(&mut self, n: usize) -> Vec<CompactNode> {
        let mut out = Vec::new();
        let mut live = 0usize;
        for c in &mut self.cands {
            if c.state == CandState::Failed {
                continue;
            }
            if live >= K || out.len() >= n {
                break;
            }
            live = live.saturating_add(1);
            if c.state == CandState::Fresh {
                c.state = CandState::InFlight;
                out.push(c.node);
            }
        }
        out
    }

    /// Records a reply. Returns true if it revealed a node closer than `best_before`.
    fn complete(
        &mut self,
        addr: SocketAddr,
        result: Result<Response, QueryError>,
        best_before: Option<Distance>,
    ) -> bool {
        let Some(cand) = self.cands.iter_mut().find(|c| c.node.addr == addr) else {
            return false;
        };
        let response = match result {
            Ok(r) if r.id == cand.node.id => r,
            _ => {
                cand.state = CandState::Failed;
                return false;
            }
        };
        cand.state = CandState::Responded;
        cand.token = response.token;
        if self.kind == Kind::GetPeers || self.kind == Kind::Scrape {
            for peer in response.values.unwrap_or_default() {
                if self.peers.len() >= MAX_LOOKUP_PEERS {
                    break;
                }
                let peer = canonical_addr(peer);
                if self.policy.dialable(&peer) && self.peer_set.insert(peer) {
                    self.peers.push(peer);
                }
            }
        }
        if self.kind == Kind::Scrape {
            match (response.bf_sd, response.bf_pe) {
                (None, None) => {
                    self.unaware = self.unaware.saturating_add(1);
                }
                (sd, pe) => {
                    self.aware = self.aware.saturating_add(1);
                    // A class the responder had no members of arrives as
                    // an empty (all-zero) filter estimating to 0; a missing
                    // half (never sent by our responder) counts the same.
                    self.seed_filters.push(
                        sd.map(|b| *b)
                            .unwrap_or([0u8; crate::bloom::BLOOM_LEN]),
                    );
                    self.peer_filters.push(
                        pe.map(|b| *b)
                            .unwrap_or([0u8; crate::bloom::BLOOM_LEN]),
                    );
                }
            }
        }
        let nodes = match self.family {
            Family::V4 => response.nodes,
            Family::V6 => response.nodes6,
        };
        let mut improved = false;
        for node in nodes
            .unwrap_or_default()
            .into_iter()
            .take(MAX_NODES_PER_RESPONSE)
        {
            if let Some(d) = self.add(node)
                && best_before.is_none_or(|b| d < b)
            {
                improved = true;
            }
        }
        improved
    }

    fn outcome(self) -> Outcome {
        let closest = self
            .cands
            .into_iter()
            .filter(|c| c.state == CandState::Responded)
            .take(K)
            .map(|c| (c.node, c.token))
            .collect();
        Outcome {
            closest,
            peers: self.peers,
        }
    }

    fn scrape_outcome(self) -> ScrapeOutcome {
        ScrapeOutcome {
            peers: self.peers,
            seed_filters: self.seed_filters,
            peer_filters: self.peer_filters,
            aware: self.aware,
            unaware: self.unaware,
        }
    }
}

async fn ask(
    inner: &Inner,
    sock: &SocketNode,
    node: CompactNode,
    kind: Kind,
    target: NodeId,
) -> (SocketAddr, Result<Response, QueryError>) {
    match kind {
        Kind::FindNode => {
            let method = Method::FindNode { target };
            (
                node.addr,
                inner.query(sock, node.addr, method, Some(node.id)).await,
            )
        }
        Kind::GetPeers => {
            let method = Method::GetPeers {
                info_hash: DhtKey::from(target),
                scrape: false,
            };
            (
                node.addr,
                inner.query(sock, node.addr, method, Some(node.id)).await,
            )
        }
        Kind::Scrape => {
            let method = Method::GetPeers {
                info_hash: DhtKey::from(target),
                scrape: true,
            };
            (
                node.addr,
                inner
                    .query_scrape(sock, node.addr, method, Some(node.id))
                    .await,
            )
        }
    }
}

async fn run(
    inner: &Inner,
    sock: &SocketNode,
    target: NodeId,
    kind: Kind,
    seeds: Vec<CompactNode>,
    deadline: Instant,
) -> Lookup {
    let tuning = &inner.cfg.tuning;
    let now = Instant::now();
    let mut lookup = Lookup::new(target, kind, sock.family, inner.policy, inner.own());
    lookup.own_ids = inner.own_ids();
    let known = lock(&sock.state).table.closest(&target, K, now, false);
    for node in known.into_iter().chain(seeds) {
        lookup.add(node);
    }

    let mut in_flight = FuturesUnordered::new();
    let mut rounds = 0usize;
    let mut final_sweep = false;
    loop {
        if Instant::now() >= deadline || inner.cancel.is_cancelled() {
            break;
        }
        let batch = lookup.pick(if final_sweep { K } else { ALPHA });
        if batch.is_empty() {
            // Nothing left to ask; late replies may still add candidates.
            let next = tokio::select! {
                next = in_flight.next() => next,
                () = tokio::time::sleep_until(deadline) => None,
            };
            match next {
                Some((addr, result)) => {
                    lookup.complete(addr, result, None);
                    continue;
                }
                None => break,
            }
        }
        rounds = rounds.saturating_add(1);
        let best_before = lookup.best_live();
        let round: HashSet<SocketAddr> = batch.iter().map(|n| n.addr).collect();
        for node in batch {
            in_flight.push(ask(inner, sock, node, kind, target));
        }
        let slow_at = after(Instant::now(), tuning.query_slow_after).min(deadline);
        let mut waiting = round.len();
        let mut answered = 0usize;
        let mut improved = false;
        while waiting > 0 {
            let next = tokio::select! {
                next = in_flight.next() => next,
                () = tokio::time::sleep_until(slow_at) => None,
            };
            let Some((addr, result)) = next else { break };
            if round.contains(&addr) {
                waiting = waiting.saturating_sub(1);
                if result.is_ok() {
                    answered = answered.saturating_add(1);
                }
            }
            improved |= lookup.complete(addr, result, best_before);
        }
        if final_sweep || rounds >= MAX_ROUNDS {
            break;
        }
        if answered > 0 && !improved {
            final_sweep = true;
        }
    }
    lookup
}

/// Iterative `get_peers` for `key`.
pub(crate) async fn get_peers(
    inner: &Inner,
    sock: &SocketNode,
    key: DhtKey,
    deadline: Instant,
) -> Outcome {
    run(
        inner,
        sock,
        NodeId::from(key),
        Kind::GetPeers,
        Vec::new(),
        deadline,
    )
    .await
    .outcome()
}

/// Iterative BEP 33 scrape for `key`: a `get_peers` traversal with
/// `scrape=1` on the dedicated scrape budget. Unions and estimation
/// happen in the caller via [`ScrapeOutcome`].
pub(crate) async fn scrape(
    inner: &Inner,
    sock: &SocketNode,
    key: DhtKey,
    deadline: Instant,
) -> ScrapeOutcome {
    run(
        inner,
        sock,
        NodeId::from(key),
        Kind::Scrape,
        Vec::new(),
        deadline,
    )
    .await
    .scrape_outcome()
}

/// Iterative `find_node` towards `target`, starting from the routing table plus `seeds`.
pub(crate) async fn find_node(
    inner: &Inner,
    sock: &SocketNode,
    target: NodeId,
    seeds: Vec<CompactNode>,
    deadline: Instant,
) -> Outcome {
    run(inner, sock, target, Kind::FindNode, seeds, deadline)
        .await
        .outcome()
}

/// `get_peers` for `key`, then `announce_peer` to the (up to K) closest nodes
/// that gave a token. Returns the number of successful replies.
pub(crate) async fn announce(
    inner: &Inner,
    sock: &SocketNode,
    key: DhtKey,
    port: u16,
    deadline: Instant,
) -> usize {
    let outcome = get_peers(inner, sock, key, deadline).await;
    let targets: Vec<(CompactNode, Vec<u8>)> = outcome
        .closest
        .into_iter()
        .filter_map(|(node, token)| token.map(|t| (node, t)))
        .take(K)
        .collect();
    let replies = join_all(targets.into_iter().map(|(node, token)| {
        let method = Method::AnnouncePeer {
            info_hash: key,
            port,
            implied_port: false,
            token, seed: false };
        inner.query(sock, node.addr, method, Some(node.id))
    }))
    .await;
    replies.iter().filter(|r| r.is_ok()).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCTION: AddrPolicy = AddrPolicy {
        allow_private: false,
        by_endpoint: false,
    };

    fn lookup(target: NodeId, kind: Kind, own: OwnAddrs) -> Lookup {
        Lookup::new(target, kind, Family::V4, PRODUCTION, Arc::new(own))
    }

    fn node(i: u8, id: NodeId) -> CompactNode {
        CompactNode {
            id,
            addr: SocketAddr::from(([8, 8, i, 1], 6881)),
        }
    }

    fn reply(id: NodeId, nodes: Vec<CompactNode>) -> Response {
        Response {
            id,
            nodes: Some(nodes),
            ..Response::default()
        }
    }

    #[test]
    fn candidates_are_sorted_filtered_and_bounded() {
        let target = NodeId([0; 20]);
        let mut own = OwnAddrs::default();
        own.add("8.8.200.1:6881".parse().unwrap());
        let mut l = lookup(target, Kind::FindNode, own);
        l.own_ids = vec![NodeId([1; 20])];
        let at = |addr: &str| CompactNode {
            id: NodeId([2; 20]),
            addr: addr.parse().unwrap(),
        };
        assert!(l.add(node(1, NodeId([1; 20]))).is_none()); // our own ID
        assert!(l.add(at("8.8.200.1:6881")).is_none()); // our own address
        assert!(l.add(at("8.8.200.1:6882")).is_none()); // our own IP
        assert!(l.add(at("10.0.0.1:1")).is_none());
        assert!(l.add(at("8.8.8.8:0")).is_none());
        assert!(l.add(at("[2a00::1]:1")).is_none());
        assert!(l.add(at("[::ffff:8.8.8.8]:1")).is_none());
        assert!(l.add(node(3, NodeId([9; 20]))).is_some());
        assert!(l.add(node(3, NodeId([8; 20]))).is_none()); // same address
        assert!(l.add(node(4, NodeId([9; 20]))).is_none()); // same ID
        assert!(l.add(node(5, NodeId([3; 20]))).is_some());
        assert_eq!(l.cands[0].node.id, NodeId([3; 20]));
        for i in 0..250u32 {
            let mut id = [0xff; 20];
            id[16..].copy_from_slice(&i.to_be_bytes());
            l.add(CompactNode {
                id: NodeId(id),
                addr: SocketAddr::from(([9, (i >> 8) as u8, i as u8, 1], 1)),
            });
        }
        assert!(l.cands.len() <= MAX_CANDIDATES);
        assert!(l.cands.windows(2).all(|w| w[0].dist <= w[1].dist));
    }

    #[test]
    fn rounds_pick_the_closest_unasked() {
        let target = NodeId([0; 20]);
        let mut l = lookup(target, Kind::GetPeers, OwnAddrs::default());
        for i in 1..=12u8 {
            l.add(node(i, NodeId([i; 20])));
        }
        let first = l.pick(ALPHA);
        assert_eq!(
            first.iter().map(|n| n.id.0[0]).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // Node 1 fails, node 2 answers with a closer node and peers, node 3 answers with nothing new.
        let best = l.best_live();
        assert!(!l.complete(first[0].addr, Err(QueryError::Timeout), best));
        let mut r = reply(NodeId([2; 20]), vec![node(50, NodeId([0; 20]))]);
        r.token = Some(b"tok".to_vec());
        r.values = Some(
            [
                "1.1.1.1:1",
                "10.0.0.1:1",
                "1.1.1.1:1",
                "[::ffff:1.1.1.1]:1",
                "[::ffff:2.2.2.2]:2",
                "1.1.1.1:0",
            ]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect(),
        );
        assert!(l.complete(first[1].addr, Ok(r), best));
        assert!(!l.complete(first[2].addr, Ok(reply(NodeId([3; 20]), vec![])), best));
        // A reply claiming a different ID counts as a failure.
        let next = l.pick(1);
        assert_eq!(next[0].id, NodeId([0; 20]));
        assert!(!l.complete(next[0].addr, Ok(reply(NodeId([7; 20]), vec![])), best));
        // The K closest live candidates bound what can still be picked.
        let rest = l.pick(K);
        assert_eq!(
            rest.iter().map(|n| n.id.0[0]).collect::<Vec<_>>(),
            vec![4, 5, 6, 7, 8, 9]
        );
        assert!(l.pick(K).is_empty());
        let outcome = l.outcome();
        let peers: Vec<SocketAddr> = ["1.1.1.1:1", "2.2.2.2:2"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(outcome.peers, peers);
        let closest: Vec<_> = outcome
            .closest
            .iter()
            .map(|(n, t)| (n.id.0[0], t.clone()))
            .collect();
        assert_eq!(closest, vec![(2, Some(b"tok".to_vec())), (3, None)]);
    }

    #[test]
    fn scrape_collects_filters_and_estimates_seeders() {
        use crate::bloom::ScrapeBloom;
        use std::net::IpAddr;

        let target = NodeId([0; 20]);
        let mut l = lookup(target, Kind::Scrape, OwnAddrs::default());
        for i in 1..=3u8 {
            l.add(node(i, NodeId([i; 20])));
        }
        let batch = l.pick(ALPHA);
        let best = l.best_live();
        // One aware response: one seed IP in BFsd, empty BFpe.
        let mut sd = ScrapeBloom::empty();
        sd.insert_ip(&"9.9.9.9".parse::<IpAddr>().unwrap());
        let mut r = reply(NodeId([1; 20]), vec![]);
        r.bf_sd = Some(Box::new(sd.0));
        r.bf_pe = Some(Box::new([0u8; crate::bloom::BLOOM_LEN]));
        assert!(!l.complete(batch[0].addr, Ok(r), best));
        // One unaware response (no filter keys) still counts as answered.
        assert!(!l.complete(batch[1].addr, Ok(reply(NodeId([2; 20]), vec![])), best));
        let out = l.scrape_outcome();
        assert_eq!(out.aware, 1);
        assert_eq!(out.unaware, 1);
        assert_eq!(out.seed_filters.len(), 1);
        assert_eq!(out.peer_filters.len(), 1);
        // One seed across the union estimates to 1; the empty peer
        // union estimates to 0, never UNKNOWN.
        assert_eq!(out.seeders_est(), Some(1));
        assert_eq!(crate::bloom::estimate_or(&out.peer_filters), Some(0));
        // No aware response at all is UNKNOWN.
        let empty = ScrapeOutcome {
            peers: Vec::new(),
            seed_filters: Vec::new(),
            peer_filters: Vec::new(),
            aware: 0,
            unaware: 3,
        };
        assert_eq!(empty.seeders_est(), None);
    }
}
