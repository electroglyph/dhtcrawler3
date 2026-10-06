//! The announce store: bounded, in memory only, never persisted (R11).
//!
//! Keys live in a vector with a position index, so a random key can be
//! picked in O(1) for BEP 51 samples.
//!
//! Abuse limits (production keying; tests key them all on IP:port):
//! * one entry per IP per key, and at most [`MAX_PEERS_PER_HOST`] per IPv6 /64;
//! * a full key replaces the oldest entry of its most represented /24 or /64;
//! * one host (IPv4 address or IPv6 /64) may create at most
//!   [`MAX_NEW_KEYS_PER_HOST`] keys, and one /24 or /48 at most
//!   [`MAX_NEW_KEYS_PER_NETWORK`], per [`NEW_KEY_WINDOW`];
//! * a full store refuses new keys instead of evicting existing ones.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::time::Duration;

use dc3_core::DhtKey;
use rand::seq::{SliceRandom, index};
use tokio::time::Instant;

use crate::compact::{AddrKey, AddrPolicy, Family};
use crate::ratelimit::{RATE_MAP_CAPACITY, WindowQuota};
use crate::util::after;

/// Maximum number of keys held (design §3).
pub(crate) const MAX_KEYS: usize = 20_000;
/// Maximum number of peers held per key (design §3).
pub(crate) const MAX_PEERS_PER_KEY: usize = 100;
/// Entries one host (IPv6 /64) may hold in one key.
pub(crate) const MAX_PEERS_PER_HOST: usize = 4;
/// New keys one host may create per window.
pub(crate) const MAX_NEW_KEYS_PER_HOST: u32 = 20;
/// New keys one /24 (IPv4) or /48 (IPv6) may create per window.
pub(crate) const MAX_NEW_KEYS_PER_NETWORK: u32 = 50;
/// The window of the new-key quotas: about two token periods.
pub(crate) const NEW_KEY_WINDOW: Duration = Duration::from_secs(600);
/// Hosts and networks tracked by the new-key quotas.
const QUOTA_CAPACITY: NonZeroUsize = RATE_MAP_CAPACITY;

/// What [`PeerStore::announce`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Announced {
    /// The peer was stored or refreshed.
    Stored,
    /// Refused: the store is full or the source used up its new-key quota.
    Refused,
}

struct Slot {
    key: DhtKey,
    /// Peer endpoint, the instant its announce expires, and whether it
    /// announced with `seed=1` (BEP 33; re-announces update the flag).
    peers: Vec<(SocketAddr, Instant, bool)>,
}

pub(crate) struct PeerStore {
    slots: Vec<Slot>,
    index: HashMap<DhtKey, usize>,
    ttl: Duration,
    max_keys: usize,
    max_peers: usize,
    policy: AddrPolicy,
    host_quota: WindowQuota,
    network_quota: WindowQuota,
}

impl PeerStore {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self::with_limits(ttl, MAX_KEYS, MAX_PEERS_PER_KEY)
    }

    pub(crate) fn with_limits(ttl: Duration, max_keys: usize, max_peers: usize) -> Self {
        Self {
            slots: Vec::new(),
            index: HashMap::new(),
            ttl,
            max_keys: max_keys.max(1),
            max_peers: max_peers.max(1),
            policy: AddrPolicy::default(),
            host_quota: WindowQuota::new(MAX_NEW_KEYS_PER_HOST, NEW_KEY_WINDOW, QUOTA_CAPACITY),
            network_quota: WindowQuota::new(
                MAX_NEW_KEYS_PER_NETWORK,
                NEW_KEY_WINDOW,
                QUOTA_CAPACITY,
            ),
        }
    }

    /// The same store keying its per-address rules with `policy`.
    pub(crate) fn with_policy(mut self, policy: AddrPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Number of keys held.
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Records that `peer` announced `key`; the peer's address is its source.
    /// `seed` is the BEP 33 `seed` flag (`seed=1` iff the announcer claims
    /// to be a seeder). A re-announce from the same IP refreshes the entry
    /// and may change the port and the seed flag.
    pub(crate) fn announce(
        &mut self,
        key: DhtKey,
        peer: SocketAddr,
        seed: bool,
        now: Instant,
    ) -> Announced {
        let expires = after(now, self.ttl);
        let policy = self.policy;
        let pos = match self.index.get(&key) {
            Some(pos) => *pos,
            None => {
                let host: AddrKey = policy.host_key(&peer);
                let network = policy.voter_key(&peer);
                if self.slots.len() >= self.max_keys
                    || self.host_quota.remaining(&host, now) == 0
                    || self.network_quota.remaining(&network, now) == 0
                {
                    return Announced::Refused;
                }
                self.host_quota.charge(host, 1, now);
                self.network_quota.charge(network, 1, now);
                self.slots.push(Slot {
                    key,
                    peers: Vec::new(),
                });
                let pos = self.slots.len().saturating_sub(1);
                self.index.insert(key, pos);
                pos
            }
        };
        let max_peers = self.max_peers;
        let Some(slot) = self.slots.get_mut(pos) else {
            return Announced::Refused;
        };
        // One entry per IP: a re-announce refreshes it and may change the port
        // and the seed flag.
        let ip_key = policy.key(&peer);
        if let Some(entry) = slot
            .peers
            .iter_mut()
            .find(|(addr, _, _)| policy.key(addr) == ip_key)
        {
            *entry = (peer, expires, seed);
            return Announced::Stored;
        }
        slot.peers.retain(|(_, exp, _)| *exp > now);
        let host = policy.host_key(&peer);
        let from_host = slot
            .peers
            .iter()
            .filter(|(addr, _, _)| policy.host_key(addr) == host)
            .count();
        let victim = if from_host >= MAX_PEERS_PER_HOST {
            oldest_where(&slot.peers, |addr| policy.host_key(addr) == host)
        } else if slot.peers.len() >= max_peers {
            // Replace the oldest announce of the most represented subnet, so
            // that one network cannot push out everyone else.
            // Ties go to the subnet with the oldest announce.
            let mut counts: HashMap<AddrKey, (usize, Instant)> = HashMap::new();
            for (addr, exp, _) in &slot.peers {
                let e = counts.entry(policy.subnet_key(addr)).or_insert((0, *exp));
                *e = (e.0.saturating_add(1), e.1.min(*exp));
            }
            let busiest = counts
                .into_iter()
                .max_by_key(|(_, (n, exp))| (*n, std::cmp::Reverse(*exp)))
                .map(|(k, _)| k);
            oldest_where(&slot.peers, |addr| Some(policy.subnet_key(addr)) == busiest)
        } else {
            None
        };
        if let Some(entry) = victim.and_then(|i| slot.peers.get_mut(i)) {
            *entry = (peer, expires, seed);
        } else if slot.peers.len() < max_peers {
            slot.peers.push((peer, expires, seed));
        } else {
            return Announced::Refused;
        }
        Announced::Stored
    }

    /// Up to `max` random live peers of `family` for `key`.
    pub(crate) fn peers(
        &self,
        key: &DhtKey,
        family: Family,
        max: usize,
        now: Instant,
    ) -> Vec<SocketAddr> {
        let Some(slot) = self.index.get(key).and_then(|pos| self.slots.get(*pos)) else {
            return Vec::new();
        };
        let mut live: Vec<SocketAddr> = slot
            .peers
            .iter()
            .filter(|(addr, exp, _)| *exp > now && Family::of(addr) == family)
            .map(|(addr, _, _)| *addr)
            .collect();
        let mut rng = rand::rng();
        live.shuffle(&mut rng);
        live.truncate(max);
        live
    }

    /// BEP 33 scrape filters for `key`: `(BFsd, BFpe)` built from the live
    /// entries of `family` (IP bytes only, no ports). `BFsd` holds the
    /// entries that announced with `seed=1`, `BFpe` the rest.
    ///
    /// Returns `None` when the key holds no live entry of `family`
    /// (a scrape to a node without local entries carries no filters, §0).
    /// A class without members yields an empty (all-zero) filter, which
    /// estimates to `0`, never UNKNOWN.
    pub(crate) fn filters(
        &self,
        key: &DhtKey,
        family: Family,
        now: Instant,
    ) -> Option<(crate::bloom::ScrapeBloom, crate::bloom::ScrapeBloom)> {
        let slot = self.index.get(key).and_then(|pos| self.slots.get(*pos))?;
        let live = slot
            .peers
            .iter()
            .filter(|(addr, exp, _)| *exp > now && Family::of(addr) == family);
        // No live entries of this family: no filters (an empty class would
        // otherwise estimate to 0 and look like a finished swarm).
        live.clone().next()?;
        let mut sd = crate::bloom::ScrapeBloom::empty();
        let mut pe = crate::bloom::ScrapeBloom::empty();
        for (addr, _, seed) in live {
            if *seed {
                sd.insert_ip(&addr.ip());
            } else {
                pe.insert_ip(&addr.ip());
            }
        }
        Some((sd, pe))
    }

    /// Up to `n` distinct random keys.
    pub(crate) fn sample(&self, n: usize) -> Vec<DhtKey> {
        let amount = n.min(self.slots.len());
        let mut rng = rand::rng();
        index::sample(&mut rng, self.slots.len(), amount)
            .into_iter()
            .filter_map(|i| self.slots.get(i).map(|s| s.key))
            .collect()
    }

    /// Drops expired announces and keys left without peers.
    pub(crate) fn expire(&mut self, now: Instant) {
        let mut pos = self.slots.len();
        while pos > 0 {
            pos = pos.saturating_sub(1);
            let empty = match self.slots.get_mut(pos) {
                Some(slot) => {
                    slot.peers.retain(|(_, exp, _)| *exp > now);
                    slot.peers.is_empty()
                }
                None => false,
            };
            if empty {
                self.remove_at(pos);
            }
        }
    }

    #[cfg(test)]
    fn contains(&self, key: &DhtKey) -> bool {
        self.index.contains_key(key)
    }

    fn remove_at(&mut self, pos: usize) {
        if pos >= self.slots.len() {
            return;
        }
        let removed = self.slots.swap_remove(pos);
        self.index.remove(&removed.key);
        if let Some(moved) = self.slots.get(pos) {
            self.index.insert(moved.key, pos);
        }
    }
}

/// Position of the entry that expires soonest among those whose address matches.
fn oldest_where(
    peers: &[(SocketAddr, Instant, bool)],
    matches: impl Fn(&SocketAddr) -> bool,
) -> Option<usize> {
    peers
        .iter()
        .enumerate()
        .filter(|(_, (addr, _, _))| matches(addr))
        .min_by_key(|(_, (_, exp, _))| *exp)
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::net::IpAddr;

    const TTL: Duration = Duration::from_secs(45 * 60);

    fn key(i: u32) -> DhtKey {
        let mut k = [0u8; 20];
        k[..4].copy_from_slice(&i.to_be_bytes());
        DhtKey(k)
    }

    fn peer(i: u32) -> SocketAddr {
        SocketAddr::from(([10, 0, (i >> 8) as u8, i as u8], 1000))
    }

    fn check_index(s: &PeerStore) {
        assert_eq!(s.index.len(), s.slots.len());
        for (i, slot) in s.slots.iter().enumerate() {
            assert_eq!(s.index[&slot.key], i);
        }
    }

    #[test]
    fn announce_and_lookup() {
        let t0 = Instant::now();
        let mut s = PeerStore::new(TTL);
        let v6: SocketAddr = "[2a00::1]:5".parse().unwrap();
        s.announce(key(1), peer(1), false, t0);
        s.announce(key(1), peer(2), false, t0);
        s.announce(key(1), peer(1), false, t0);
        s.announce(key(1), v6, false, t0);
        assert_eq!(s.len(), 1);
        let got: HashSet<_> = s.peers(&key(1), Family::V4, 10, t0).into_iter().collect();
        assert_eq!(got, HashSet::from([peer(1), peer(2)]));
        assert_eq!(s.peers(&key(1), Family::V6, 10, t0), vec![v6]);
        assert_eq!(s.peers(&key(1), Family::V4, 1, t0).len(), 1);
        assert!(s.peers(&key(2), Family::V4, 10, t0).is_empty());
    }

    #[test]
    fn announces_expire() {
        let t0 = Instant::now();
        let mut s = PeerStore::new(TTL);
        s.announce(key(1), peer(1), false, t0);
        s.announce(key(2), peer(2), false, t0 + Duration::from_secs(600));
        let late = t0 + TTL + Duration::from_secs(1);
        assert!(s.peers(&key(1), Family::V4, 10, late).is_empty());
        assert_eq!(s.peers(&key(2), Family::V4, 10, late), vec![peer(2)]);
        s.expire(late);
        assert_eq!(s.len(), 1);
        check_index(&s);
        // Re-announcing refreshes the expiry.
        s.announce(key(2), peer(2), false, late);
        s.expire(t0 + TTL + Duration::from_secs(900));
        assert_eq!(s.len(), 1);
        s.expire(late + TTL);
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn bounded_keys_and_peers() {
        let t0 = Instant::now();
        let mut s = PeerStore::with_limits(TTL, 50, 3);
        for i in 0..200 {
            s.announce(key(i), peer(i * 256), false, t0);
            check_index(&s);
        }
        assert_eq!(s.len(), 50);
        // A full store keeps its keys.
        assert!(s.contains(&key(0)) && !s.contains(&key(199)));
        for i in 0..10 {
            s.announce(
                key(0),
                peer(1000 + i),
                false,
                t0 + Duration::from_secs(u64::from(i)),
            );
        }
        let peers = s.peers(&key(0), Family::V4, 100, t0 + Duration::from_secs(20));
        assert_eq!(peers.len(), 3);
        // The newcomers' /24 is the most represented, so they replace each
        // other and the first announcer (another /24) stays.
        let set: HashSet<_> = peers.into_iter().collect();
        assert_eq!(set, HashSet::from([peer(0), peer(1008), peer(1009)]));
    }

    fn global(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::from((ip, port))
    }

    #[test]
    fn one_ip_cannot_take_all_peer_slots_of_a_key() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let mut s = PeerStore::new(TTL);
        for i in 0..50u8 {
            s.announce(key(1), global([9, i, 0, 1], 6881), false, t0);
        }
        for port in 1..=200 {
            s.announce(key(1), global([6, 6, 6, 6], port), false, t1);
        }
        let peers = s.peers(&key(1), Family::V4, 200, t1);
        let attacker = peers
            .iter()
            .filter(|p| p.ip() == IpAddr::from([6, 6, 6, 6]))
            .count();
        assert_eq!(attacker, 1);
        assert_eq!(peers.len(), 51);
    }

    #[test]
    fn one_source_cannot_announce_unlimited_keys() {
        let t0 = Instant::now();
        let mut s = PeerStore::new(TTL);
        for i in 0..60 {
            s.announce(key(i), global([6, 6, 6, 6], 6881), false, t0);
        }
        assert!(s.len() <= 20, "{} keys from one IP", s.len());
        // Rotating addresses inside one IPv6 /64 does not help.
        let before = s.len();
        for i in 0..60u16 {
            let addr = SocketAddr::new(
                std::net::Ipv6Addr::new(0x2a01, 0x4f8, 1, 2, i, 0, 0, 1).into(),
                6881,
            );
            s.announce(key(100 + u32::from(i)), addr, false, t0);
        }
        assert!(
            s.len() - before <= 20,
            "{} keys from one /64",
            s.len() - before
        );
        // Nor do many IPv4 addresses in one /24.
        let before = s.len();
        for i in 0..200u32 {
            s.announce(
                key(1000 + i),
                global([7, 7, 7, (i % 250) as u8], 6881),
                false,
                t0,
            );
        }
        assert!(
            s.len() - before <= 50,
            "{} keys from one /24",
            s.len() - before
        );
    }

    #[test]
    fn full_store_keeps_its_keys() {
        let t0 = Instant::now();
        let mut s = PeerStore::with_limits(TTL, 50, 3);
        for i in 0..50 {
            s.announce(key(i), global([9, 0, i as u8, 1], 6881), false, t0);
        }
        s.announce(key(999), global([9, 1, 0, 1], 6881), false, t0);
        assert_eq!(s.len(), 50);
        assert!(!s.index.contains_key(&key(999)));
        assert!((0..50).all(|i| s.index.contains_key(&key(i))));
    }

    #[test]
    fn seed_flag_is_stored_and_filters_split_seeds_from_peers() {
        let t0 = Instant::now();
        let mut s = PeerStore::new(TTL);
        assert!(s.filters(&key(1), Family::V4, t0).is_none());
        s.announce(key(1), peer(1), true, t0);
        s.announce(key(1), peer(2), false, t0);
        let (sd, pe) = s.filters(&key(1), Family::V4, t0).expect("entries exist");
        let seeds = sd.estimate().expect("not saturated");
        let peers = pe.estimate().expect("not saturated");
        assert!((seeds - 1.0).abs() < 0.1, "one seed, got {seeds}");
        assert!((peers - 1.0).abs() < 0.1, "one peer, got {peers}");
        // A re-announce from the same IP flips the flag.
        s.announce(key(1), peer(1), false, t0);
        let (sd, pe) = s.filters(&key(1), Family::V4, t0).expect("entries exist");
        assert_eq!(sd.estimate(), Some(0.0));
        let peers = pe.estimate().expect("not saturated");
        assert!((peers - 2.0).abs() < 0.2, "two peers, got {peers}");
        // Filters are per family: no IPv6 entries here.
        assert!(s.filters(&key(1), Family::V6, t0).is_none());
    }

    #[test]
    fn samples_are_distinct_and_bounded() {
        let t0 = Instant::now();
        let mut s = PeerStore::new(TTL);
        assert!(s.sample(20).is_empty());
        for i in 0..5 {
            s.announce(key(i), peer(i), false, t0);
        }
        let all: HashSet<_> = s.sample(20).into_iter().collect();
        assert_eq!(all.len(), 5);
        for i in 5..100 {
            s.announce(key(i), peer(i), false, t0);
        }
        let some = s.sample(20);
        assert_eq!(some.len(), 20);
        assert_eq!(some.iter().collect::<HashSet<_>>().len(), 20);
    }
}
