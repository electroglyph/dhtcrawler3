//! Sockets, the transaction table and external-IP voting.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::num::NonZeroUsize;
use std::time::Duration;

use lru::LruCache;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::compact::{AddrKey, canonical_ip, is_global_ip};
use crate::krpc::{KrpcError, Response};
use crate::node_id::NodeId;

/// Length of the transaction IDs we send.
pub(crate) const TID_LEN: usize = 2;
/// Queries waiting for a reply, per socket.
pub(crate) const MAX_PENDING_PER_SOCKET: usize = 4096;
/// Attempts to find an unused transaction ID for one endpoint.
const TID_ATTEMPTS: usize = 16;
/// External-IP votes kept per socket: the most recent voters.
pub(crate) const MAX_IP_VOTES: NonZeroUsize = NonZeroUsize::MIN.saturating_add(1023);
/// A winning address needs more than `NUM / DEN` of the current votes.
const VOTE_MAJORITY_NUM: usize = 2;
/// See [`VOTE_MAJORITY_NUM`].
const VOTE_MAJORITY_DEN: usize = 3;
/// Link-local IPv6 prefix (fe80::/10); only these addresses keep a scope ID.
const V6_LINK_LOCAL: (u16, u16) = (0xfe80, 0xffc0);
/// A public IPv6 address used to find our IPv6 source address; nothing is
/// sent to it (Google Public DNS, port 53).
const V6_PROBE_TARGET: SocketAddr = SocketAddr::V6(SocketAddrV6::new(
    Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888),
    53,
    0,
    0,
));

/// What a waiting query receives.
#[derive(Debug)]
pub(crate) enum Reply {
    Response(Response),
    Error(KrpcError),
    /// The reply matched but could not be parsed.
    Malformed,
}

pub(crate) struct Pending {
    pub(crate) tx: oneshot::Sender<Reply>,
    pub(crate) deadline: Instant,
    /// The node ID we expect to answer, when known.
    pub(crate) expect: Option<NodeId>,
}

/// Outstanding queries of one socket, keyed by (transaction ID, endpoint).
pub(crate) struct Transactions {
    map: HashMap<([u8; TID_LEN], SocketAddr), Pending>,
    /// Outstanding queries per endpoint.
    per_endpoint: HashMap<SocketAddr, usize>,
    max: usize,
}

impl Transactions {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            map: HashMap::new(),
            per_endpoint: HashMap::new(),
            max,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Registers a query to `addr` and returns its transaction ID, or `None`
    /// when the table is full.
    pub(crate) fn insert(
        &mut self,
        addr: SocketAddr,
        pending: Pending,
        now: Instant,
    ) -> Option<[u8; TID_LEN]> {
        if self.map.len() >= self.max {
            self.sweep(now);
            if self.map.len() >= self.max {
                return None;
            }
        }
        for _ in 0..TID_ATTEMPTS {
            let tid: [u8; TID_LEN] = rand::random();
            if let Entry::Vacant(slot) = self.map.entry((tid, addr)) {
                slot.insert(pending);
                let n = self.per_endpoint.entry(addr).or_insert(0);
                *n = n.saturating_add(1);
                return Some(tid);
            }
        }
        None
    }

    /// Removes and returns the query matching a reply. Replies with an
    /// unknown (ID, endpoint) pair match nothing.
    pub(crate) fn take(&mut self, tid: &[u8], from: &SocketAddr) -> Option<Pending> {
        let tid: [u8; TID_LEN] = tid.try_into().ok()?;
        let pending = self.map.remove(&(tid, *from))?;
        Self::forget(&mut self.per_endpoint, from);
        Some(pending)
    }

    pub(crate) fn remove(&mut self, tid: [u8; TID_LEN], addr: &SocketAddr) {
        if self.map.remove(&(tid, *addr)).is_some() {
            Self::forget(&mut self.per_endpoint, addr);
        }
    }

    /// Whether a query to `addr` is outstanding.
    pub(crate) fn expects(&self, addr: &SocketAddr) -> bool {
        self.per_endpoint.contains_key(addr)
    }

    fn forget(per_endpoint: &mut HashMap<SocketAddr, usize>, addr: &SocketAddr) {
        if let Entry::Occupied(mut e) = per_endpoint.entry(*addr) {
            *e.get_mut() = e.get().saturating_sub(1);
            if *e.get() == 0 {
                e.remove();
            }
        }
    }

    /// Drops expired queries and queries nobody waits for. Returns how many.
    pub(crate) fn sweep(&mut self, now: Instant) -> usize {
        let before = self.map.len();
        let per_endpoint = &mut self.per_endpoint;
        self.map.retain(|(_, addr), p| {
            let keep = p.deadline > now && !p.tx.is_closed();
            if !keep {
                Self::forget(per_endpoint, addr);
            }
            keep
        });
        before.saturating_sub(self.map.len())
    }
}

/// External-IP voting (design §7). Only the top-level `ip` of responses to
/// our own queries is recorded. Each voter (a /24 or /48, see
/// `AddrPolicy::voter_key`) has one vote, and its latest vote replaces the
/// earlier one. Votes expire after `ttl`. A candidate wins with at least
/// `min_votes` current votes and more than two-thirds of them.
///
/// Accepted bound: at most [`MAX_IP_VOTES`] voters are kept and the oldest
/// vote makes room when full, so an attacker with more distinct networks
/// than that can flush honest votes before they win. This is inherent to a
/// bounded in-memory vote without proof of work; the per-network voter key
/// (not per host) is what keeps the cost of such a flood at one network
/// per vote.
pub(crate) struct IpVoter {
    /// Voter → (address, time of the vote); the LRU order is the vote order.
    votes: LruCache<AddrKey, (IpAddr, Instant)>,
    /// Address → current votes for it.
    tally: HashMap<IpAddr, usize>,
    min_votes: usize,
    ttl: Duration,
}

impl IpVoter {
    pub(crate) fn new(min_votes: usize, ttl: Duration) -> Self {
        Self::with_capacity(min_votes, ttl, MAX_IP_VOTES)
    }

    pub(crate) fn with_capacity(min_votes: usize, ttl: Duration, capacity: NonZeroUsize) -> Self {
        Self {
            votes: LruCache::new(capacity),
            tally: HashMap::new(),
            min_votes: min_votes.max(1),
            ttl,
        }
    }

    /// Records that `voter` saw us at `ip` and returns the winner, if any.
    pub(crate) fn record(&mut self, voter: AddrKey, ip: IpAddr, now: Instant) -> Option<IpAddr> {
        self.expire(now);
        let ip = canonical_ip(ip);
        // `push` returns the voter's earlier vote, or the oldest vote when full.
        if let Some((_, (old, _))) = self.votes.push(voter, (ip, now)) {
            self.untally(old);
        }
        let count = self.tally.entry(ip).or_insert(0);
        *count = count.saturating_add(1);
        self.winner()
    }

    /// The address with at least `min_votes` votes and more than two-thirds
    /// of the current votes, if there is one.
    pub(crate) fn winner(&self) -> Option<IpAddr> {
        let total = self.votes.len();
        let (ip, count) = self.tally.iter().max_by_key(|(_, n)| **n)?;
        let enough = *count >= self.min_votes;
        let majority =
            count.saturating_mul(VOTE_MAJORITY_DEN) > total.saturating_mul(VOTE_MAJORITY_NUM);
        (enough && majority).then_some(*ip)
    }

    /// Number of current votes.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.votes.len()
    }

    /// Drops votes older than `ttl`.
    fn expire(&mut self, now: Instant) {
        while let Some(cast_at) = self.votes.peek_lru().map(|(_, (_, at))| *at) {
            if now.saturating_duration_since(cast_at) < self.ttl {
                break;
            }
            if let Some((_, (ip, _))) = self.votes.pop_lru() {
                self.untally(ip);
            }
        }
    }

    fn untally(&mut self, ip: IpAddr) {
        if let Entry::Occupied(mut entry) = self.tally.entry(ip) {
            let count = entry.get_mut();
            *count = count.saturating_sub(1);
            if *count == 0 {
                entry.remove();
            }
        }
    }
}

/// Binds a UDP socket; IPv6 sockets are IPv6-only (BEP 32 runs two tables).
pub(crate) fn bind_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

/// Whether the host has a global IPv6 address. Connecting an unbound UDP
/// socket makes the kernel pick a source address; no packet is sent.
pub(crate) fn has_global_ipv6() -> bool {
    let probe = || -> io::Result<Option<SocketAddr>> {
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_only_v6(true)?;
        socket.connect(&V6_PROBE_TARGET.into())?;
        Ok(socket.local_addr()?.as_socket())
    };
    matches!(probe(), Ok(Some(local)) if local.is_ipv6() && is_global_ip(local.ip()))
}

/// Canonical form of a datagram source: no flow label, and a scope ID only
/// for link-local IPv6 addresses. Keeps transaction matching exact.
pub(crate) fn normalize_addr(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(_) => addr,
        SocketAddr::V6(v6) => {
            let (prefix, mask) = V6_LINK_LOCAL;
            let link_local = v6.ip().segments()[0] & mask == prefix;
            let scope = if link_local { v6.scope_id() } else { 0 };
            SocketAddr::V6(SocketAddrV6::new(*v6.ip(), v6.port(), 0, scope))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::AddrPolicy;

    const PRODUCTION: AddrPolicy = AddrPolicy {
        allow_private: false,
        by_endpoint: false,
    };
    const MIN: Duration = Duration::from_secs(60);
    const TTL: Duration = Duration::from_secs(30 * 60);

    fn pending(deadline: Instant) -> (Pending, oneshot::Receiver<Reply>) {
        let (tx, rx) = oneshot::channel();
        (
            Pending {
                tx,
                deadline,
                expect: None,
            },
            rx,
        )
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The production voter key of a responder at `addr`.
    fn voter(addr: &str) -> AddrKey {
        PRODUCTION.voter_key(&format!("{addr}:6881").parse().unwrap())
    }

    /// A responder in the `i`-th distinct /24.
    fn net(i: u32) -> AddrKey {
        voter(&format!("9.{}.{}.1", i >> 8, i & 0xff))
    }

    #[test]
    fn transactions_match_id_and_endpoint() {
        let now = Instant::now();
        let mut t = Transactions::new(8);
        let a: SocketAddr = "8.8.8.8:1".parse().unwrap();
        let b: SocketAddr = "8.8.8.8:2".parse().unwrap();
        let (p, _rx) = pending(now + Duration::from_secs(4));
        let tid = t.insert(a, p, now).unwrap();
        assert_eq!(t.len(), 1);
        // Same ID from another endpoint, or a wrong-length ID: ignored.
        assert!(t.take(&tid, &b).is_none());
        assert!(t.take(&[tid[0]], &a).is_none());
        assert!(t.take(&[tid[0], tid[1], 0], &a).is_none());
        assert!(t.expects(&a) && !t.expects(&b));
        assert!(t.take(&tid, &a).is_some());
        assert!(t.take(&tid, &a).is_none());
        assert_eq!(t.len(), 0);
        assert!(!t.expects(&a));
        let (p, _rx) = pending(now + Duration::from_secs(4));
        let tid = t.insert(a, p, now).unwrap();
        t.remove(tid, &a);
        t.remove(tid, &a);
        assert!(!t.expects(&a));
    }

    #[test]
    fn transactions_are_bounded_and_swept() {
        let now = Instant::now();
        let mut t = Transactions::new(4);
        let a: SocketAddr = "8.8.8.8:1".parse().unwrap();
        let mut keep = Vec::new();
        for i in 0..4 {
            let (p, rx) = pending(now + Duration::from_secs(i + 1));
            assert!(t.insert(a, p, now).is_some());
            keep.push(rx);
        }
        let (p, _rx) = pending(now + Duration::from_secs(9));
        assert!(t.insert(a, p, now).is_none());
        // After the first deadline passes, a slot frees up.
        let (p, _rx2) = pending(now + Duration::from_secs(9));
        assert!(t.insert(a, p, now + Duration::from_secs(1)).is_some());
        assert_eq!(t.len(), 4);
        // Entries whose waiter is gone are swept too; `_rx2` is still waiting.
        keep.clear();
        assert_eq!(t.sweep(now + Duration::from_secs(1)), 3);
        assert_eq!(t.len(), 1);
        assert!(t.expects(&a));
        assert_eq!(t.sweep(now + Duration::from_secs(9)), 1);
        assert!(!t.expects(&a) && t.per_endpoint.is_empty());
    }

    #[test]
    fn ten_distinct_networks_win() {
        let t0 = Instant::now();
        let mut v = IpVoter::new(10, TTL);
        let ours = ip("1.2.3.4");
        for i in 0..9 {
            assert_eq!(v.record(net(i), ours, t0), None, "vote {i}");
        }
        assert_eq!(v.record(net(9), ours, t0), Some(ours));
        assert_eq!(v.winner(), Some(ours));
    }

    #[test]
    fn nine_networks_cannot_win() {
        let t0 = Instant::now();
        let mut v = IpVoter::new(10, TTL);
        let evil = ip("6.6.6.6");
        // Many hosts in 9 networks, voting again and again.
        for round in 0..20u32 {
            for i in 0..9u32 {
                let host = voter(&format!("9.0.{i}.{}", 1 + round));
                assert_eq!(
                    v.record(host, evil, t0 + Duration::from_secs(u64::from(round))),
                    None
                );
            }
        }
        assert_eq!(v.len(), 9);
        // Ten honest networks then outvote them, but only with a two-thirds majority.
        let ours = ip("1.2.3.4");
        for i in 0..10 {
            assert_eq!(v.record(net(100 + i), ours, t0 + MIN), None);
        }
        for i in 10..18 {
            assert_eq!(
                v.record(net(100 + i), ours, t0 + MIN),
                None,
                "honest vote {i}"
            );
        }
        // 19 of 28 votes is more than two-thirds.
        assert_eq!(v.record(net(118), ours, t0 + MIN), Some(ours));
    }

    #[test]
    fn one_network_counts_once() {
        let t0 = Instant::now();
        let mut v = IpVoter::new(10, TTL);
        let ours = ip("1.2.3.4");
        for host in 0..100u32 {
            let addr = format!("9.9.9.{}", host % 256);
            assert_eq!(v.record(voter(&addr), ours, t0), None);
        }
        assert_eq!(v.len(), 1);
        // The same holds for an IPv6 /48.
        let mut v6 = IpVoter::new(10, TTL);
        let ours6 = ip("2a00:1:2:3::1");
        for host in 0..100u32 {
            let addr = format!("[2a00:9:9:{host:x}::1]");
            assert_eq!(v6.record(voter(&addr), ours6, t0), None);
        }
        assert_eq!(v6.len(), 1);
    }

    #[test]
    fn latest_vote_replaces_earlier_one() {
        let t0 = Instant::now();
        let mut v = IpVoter::new(1, TTL);
        let (a, b) = (ip("1.1.1.1"), ip("2.2.2.2"));
        assert_eq!(v.record(net(1), a, t0), Some(a));
        assert_eq!(v.record(net(1), b, t0), Some(b));
        assert_eq!(v.len(), 1);
        assert_eq!(v.tally.get(&a), None);
        assert_eq!(v.tally.get(&b), Some(&1));
        // IPv4-mapped reports count as the IPv4 address.
        assert_eq!(v.record(net(1), ip("::ffff:1.1.1.1"), t0), Some(a));
    }

    #[test]
    fn votes_expire() {
        let t0 = Instant::now();
        let mut v = IpVoter::new(10, TTL);
        let ours = ip("1.2.3.4");
        for i in 0..9 {
            v.record(net(i), ours, t0);
        }
        // The tenth vote 29 minutes later still wins.
        assert_eq!(v.record(net(9), ours, t0 + 29 * MIN), Some(ours));
        // At 30 minutes the first nine have expired.
        assert_eq!(v.record(net(10), ours, t0 + 30 * MIN), None);
        assert_eq!(v.len(), 2);
        assert_eq!(v.tally.get(&ours), Some(&2));
        // A refreshed vote lives on.
        assert_eq!(v.record(net(9), ours, t0 + 50 * MIN), None);
        assert_eq!(v.len(), 2);
        assert_eq!(v.record(net(11), ours, t0 + 60 * MIN), None);
        assert_eq!(v.len(), 2);
        // Everything is gone after a long silence.
        v.record(net(12), ours, t0 + 200 * MIN);
        assert_eq!(v.len(), 1);
        assert_eq!(v.tally.len(), 1);
    }

    #[test]
    fn two_thirds_tie_does_not_win() {
        let t0 = Instant::now();
        let mut v = IpVoter::new(10, TTL);
        let (ours, other) = (ip("1.2.3.4"), ip("5.6.7.8"));
        for i in 0..10 {
            v.record(net(i), other, t0);
        }
        assert_eq!(v.winner(), Some(other));
        for i in 10..29 {
            assert_ne!(v.record(net(i), ours, t0), Some(ours));
        }
        // 20 of 30 is exactly two-thirds: not enough, and 10 of 30 is not either.
        assert_eq!(v.record(net(29), ours, t0), None);
        assert_eq!(v.tally.get(&ours), Some(&20));
        // 21 of 31 is more.
        assert_eq!(v.record(net(30), ours, t0), Some(ours));
    }

    #[test]
    fn voter_map_is_bounded() {
        let t0 = Instant::now();
        let mut v = IpVoter::with_capacity(3, TTL, NonZeroUsize::new(4).unwrap());
        let (a, b) = (ip("1.1.1.1"), ip("2.2.2.2"));
        for i in 0..10 {
            v.record(net(i), a, t0);
        }
        assert_eq!(v.len(), 4);
        assert_eq!(v.tally.get(&a), Some(&4));
        // The oldest votes make room and are no longer counted.
        for i in 10..13 {
            v.record(net(i), b, t0);
        }
        assert_eq!(v.tally.get(&a), Some(&1));
        assert_eq!(v.winner(), Some(b));
    }

    #[test]
    fn a_flood_past_capacity_flushes_honest_votes() {
        // Accepted bound (see `IpVoter` docs): more distinct networks than
        // capacity evicts honest votes, so they can never win. The flood
        // costs one network per vote.
        let t0 = Instant::now();
        let mut v = IpVoter::with_capacity(10, TTL, NonZeroUsize::new(12).unwrap());
        let (ours, evil) = (ip("1.2.3.4"), ip("6.6.6.6"));
        for i in 0..9 {
            assert_eq!(v.record(net(i), ours, t0), None);
        }
        for i in 100..112 {
            v.record(net(i), evil, t0);
        }
        assert_eq!(v.tally.get(&ours), None);
        assert_eq!(v.winner(), Some(evil));
    }

    #[test]
    fn endpoint_voters_in_tests() {
        let t0 = Instant::now();
        let policy = AddrPolicy {
            allow_private: true,
            by_endpoint: true,
        };
        let mut v = IpVoter::new(2, TTL);
        let lo = ip("127.0.0.1");
        assert_eq!(
            v.record(policy.voter_key(&"127.0.0.1:1".parse().unwrap()), lo, t0),
            None
        );
        assert_eq!(
            v.record(policy.voter_key(&"127.0.0.1:2".parse().unwrap()), lo, t0),
            Some(lo)
        );
    }

    #[test]
    fn normalize() {
        let a = SocketAddr::V6(SocketAddrV6::new("2a00::1".parse().unwrap(), 5, 7, 3));
        assert_eq!(
            normalize_addr(a),
            "[2a00::1]:5".parse::<SocketAddr>().unwrap()
        );
        let ll = SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 5, 7, 3));
        assert_eq!(
            normalize_addr(ll),
            SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 5, 0, 3))
        );
        let v4: SocketAddr = "1.2.3.4:5".parse().unwrap();
        assert_eq!(normalize_addr(v4), v4);
    }

    #[tokio::test]
    async fn binds_both_families() {
        let s4 = bind_udp("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(s4.local_addr().unwrap().is_ipv4());
        // IPv6 may be unavailable in some containers; only check it when it works.
        if let Ok(s6) = bind_udp("[::1]:0".parse().unwrap()) {
            assert!(s6.local_addr().unwrap().is_ipv6());
        }
        // An address the host does not have cannot be bound.
        assert!(bind_udp("[2001:db8::1]:0".parse().unwrap()).is_err());
    }

    #[test]
    fn global_ipv6_probe_does_not_fail() {
        // The answer depends on the host; the probe must simply return.
        let _ = has_global_ipv6();
    }
}
