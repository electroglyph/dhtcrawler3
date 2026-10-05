//! Kademlia routing table (BEP 5) as a pure data structure.
//!
//! Time is passed in, so every rule is unit-testable:
//! * K = 8 per bucket; only the bucket that holds our own ID splits.
//! * Nodes are good, questionable or bad (BEP 5). Bad nodes are replaced from
//!   a per-bucket replacement cache, which prefers confirmed, BEP 42-valid
//!   and recently active nodes.
//! * One entry per IP in the whole table, and no two entries from the same
//!   /24 (IPv4) or /64 (IPv6) in one bucket. With `limits_by_endpoint`
//!   (tests only) both rules key on IP:port instead.
//! * Our own ID, our own addresses, other families, IPv4-mapped IPv6
//!   addresses and non-dialable addresses are never added. Callers keep
//!   read-only (BEP 43) senders and bootstrap routers out.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::compact::{AddrKey, AddrPolicy, CompactNode, Family, OwnAddrs, canonical_addr};
use crate::node_id::{Distance, ID_BITS, NodeId, is_bep42_exempt};

/// Bucket size (BEP 5).
pub(crate) const K: usize = 8;
/// Replacement candidates kept per bucket.
pub(crate) const REPLACEMENTS_PER_BUCKET: usize = 8;
/// Consecutive failures after which a node that has answered before is bad.
pub(crate) const MAX_FAILURES: u8 = 3;
/// Consecutive failures after which a node that never answered is bad.
pub(crate) const MAX_FAILURES_UNCONFIRMED: u8 = 1;
/// A bad node is dropped after this many failures even with no replacement.
pub(crate) const FORGET_AFTER_FAILURES: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Good,
    Questionable,
    Bad,
}

/// The evidence behind an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The node answered one of our queries.
    Response,
    /// The node sent us a query.
    Query,
}

#[derive(Clone, Debug)]
pub(crate) struct NodeEntry {
    pub(crate) id: NodeId,
    pub(crate) addr: SocketAddr,
    /// Whether the ID is BEP 42-valid for the address.
    pub(crate) bep42: bool,
    pub(crate) last_response: Option<Instant>,
    pub(crate) last_query: Option<Instant>,
    pub(crate) last_ping: Option<Instant>,
    pub(crate) failures: u8,
}

impl NodeEntry {
    fn new(id: NodeId, addr: SocketAddr, bep42: bool) -> Self {
        Self {
            id,
            addr,
            bep42,
            last_response: None,
            last_query: None,
            last_ping: None,
            failures: 0,
        }
    }

    fn confirmed(&self) -> bool {
        self.last_response.is_some()
    }

    fn last_active(&self) -> Option<Instant> {
        self.last_response.max(self.last_query)
    }

    fn mark(&mut self, seen: Seen, now: Instant) {
        match seen {
            Seen::Response => {
                self.last_response = Some(now);
                self.failures = 0;
            }
            Seen::Query => self.last_query = Some(now),
        }
    }

    /// Folds the timestamps of `other` (same node) into `self`.
    fn merge(&mut self, other: &NodeEntry) {
        self.last_response = self.last_response.max(other.last_response);
        self.last_query = self.last_query.max(other.last_query);
        if other.confirmed() {
            self.failures = self.failures.min(other.failures);
        }
    }

    pub(crate) fn node(&self) -> CompactNode {
        CompactNode {
            id: self.id,
            addr: self.addr,
        }
    }

    /// Replacement preference: confirmed, then BEP 42-valid, then most recently active.
    fn rank(&self) -> (bool, bool, Option<Instant>) {
        (self.confirmed(), self.bep42, self.last_active())
    }
}

fn status_of(e: &NodeEntry, now: Instant, questionable_after: Duration) -> Status {
    let limit = if e.confirmed() {
        MAX_FAILURES
    } else {
        MAX_FAILURES_UNCONFIRMED
    };
    if e.failures >= limit {
        return Status::Bad;
    }
    if e.failures > 0 {
        return Status::Questionable;
    }
    let recent = |t: Option<Instant>| {
        t.is_some_and(|t| now.saturating_duration_since(t) < questionable_after)
    };
    if recent(e.last_response) || (e.confirmed() && recent(e.last_query)) {
        Status::Good
    } else {
        Status::Questionable
    }
}

struct Bucket {
    nodes: Vec<NodeEntry>,
    replacements: Vec<NodeEntry>,
    last_changed: Instant,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self {
            nodes: Vec::with_capacity(K),
            replacements: Vec::new(),
            last_changed: now,
        }
    }
}

/// What a routing table needs to know besides its nodes.
#[derive(Clone, Debug)]
pub(crate) struct TableConfig {
    pub(crate) family: Family,
    pub(crate) questionable_after: Duration,
    pub(crate) policy: AddrPolicy,
    pub(crate) own: Arc<OwnAddrs>,
}

pub(crate) struct RoutingTable {
    own_id: NodeId,
    /// Bucket `i` holds nodes sharing exactly `i` prefix bits with `own_id`;
    /// the last bucket holds every node sharing at least that many.
    buckets: Vec<Bucket>,
    /// Address key → ID of the member using it.
    by_addr: HashMap<AddrKey, NodeId>,
    questionable_after: Duration,
    cfg: TableConfig,
}

impl RoutingTable {
    pub(crate) fn new(own_id: NodeId, now: Instant, cfg: TableConfig) -> Self {
        Self {
            own_id,
            buckets: vec![Bucket::new(now)],
            by_addr: HashMap::new(),
            questionable_after: cfg.questionable_after,
            cfg,
        }
    }

    fn key(&self, addr: &SocketAddr) -> AddrKey {
        self.cfg.policy.key(addr)
    }

    fn same_subnet(&self, a: &SocketAddr, b: &SocketAddr) -> bool {
        self.cfg.policy.same_subnet(a, b)
    }

    /// Whether `addr` may be a member: our family, canonical, dialable and not ours.
    pub(crate) fn admissible(&self, addr: &SocketAddr) -> bool {
        Family::of(addr) == self.cfg.family
            && canonical_addr(*addr) == *addr
            && self.cfg.policy.dialable(addr)
            && !self.cfg.own.contains(addr, self.cfg.policy)
    }

    /// Replaces our own addresses and drops members and candidates that use one.
    pub(crate) fn set_own(&mut self, own: Arc<OwnAddrs>, now: Instant) {
        let policy = self.cfg.policy;
        for bucket in &mut self.buckets {
            bucket
                .replacements
                .retain(|r| !own.contains(&r.addr, policy));
        }
        let ours: Vec<NodeId> = self
            .members()
            .filter(|e| own.contains(&e.addr, policy))
            .map(|e| e.id)
            .collect();
        self.cfg.own = own;
        for id in ours {
            let idx = self.bucket_index(&id);
            self.remove_member(&id);
            self.fill_from_replacements(idx, now);
        }
    }

    #[cfg(test)]
    pub(crate) fn own_id(&self) -> NodeId {
        self.own_id
    }

    /// Number of members (replacement candidates are not counted).
    pub(crate) fn len(&self) -> usize {
        self.by_addr.len()
    }

    #[cfg(test)]
    pub(crate) fn num_buckets(&self) -> usize {
        self.buckets.len()
    }

    pub(crate) fn status(&self, e: &NodeEntry, now: Instant) -> Status {
        status_of(e, now, self.questionable_after)
    }

    pub(crate) fn good_count(&self, now: Instant) -> usize {
        self.members()
            .filter(|e| self.status(e, now) == Status::Good)
            .count()
    }

    pub(crate) fn members(&self) -> impl Iterator<Item = &NodeEntry> {
        self.buckets.iter().flat_map(|b| b.nodes.iter())
    }

    fn last_index(&self) -> usize {
        self.buckets.len().saturating_sub(1)
    }

    fn bucket_index(&self, id: &NodeId) -> usize {
        self.own_id.common_prefix_len(id).min(self.last_index())
    }

    fn member_mut(&mut self, id: &NodeId) -> Option<&mut NodeEntry> {
        let idx = self.bucket_index(id);
        self.buckets
            .get_mut(idx)?
            .nodes
            .iter_mut()
            .find(|n| n.id == *id)
    }

    fn member(&self, id: &NodeId) -> Option<&NodeEntry> {
        self.buckets
            .get(self.bucket_index(id))?
            .nodes
            .iter()
            .find(|n| n.id == *id)
    }

    /// A node answered one of our queries. Returns true if it is now a member.
    pub(crate) fn on_response(
        &mut self,
        id: NodeId,
        addr: SocketAddr,
        bep42: bool,
        now: Instant,
    ) -> bool {
        self.upsert(NodeEntry::new(id, addr, bep42), Some(Seen::Response), now)
    }

    /// A (non-read-only) node sent us a query. Returns true if it is a member.
    pub(crate) fn on_query(
        &mut self,
        id: NodeId,
        addr: SocketAddr,
        bep42: bool,
        now: Instant,
    ) -> bool {
        self.upsert(NodeEntry::new(id, addr, bep42), Some(Seen::Query), now)
    }

    /// Inserts or updates a node. `seen` is `None` when `cand` already carries
    /// its timestamps (table rebuilds).
    fn upsert(&mut self, mut cand: NodeEntry, seen: Option<Seen>, now: Instant) -> bool {
        if cand.id == self.own_id || !self.admissible(&cand.addr) {
            return false;
        }
        if let Some(seen) = seen {
            cand.mark(seen, now);
        }
        // BEP 42: a sender whose address is public but whose ID is not valid
        // for it never becomes a full member; it waits in the replacements
        // cache. Otherwise one IP could claim arbitrarily close IDs.
        if !cand.bep42 && !is_bep42_exempt(cand.addr.ip()) {
            let idx = self.bucket_index(&cand.id);
            self.add_replacement(idx, cand);
            return false;
        }
        // Only a node that has answered us may displace or move existing entries.
        let proven = match seen {
            Some(s) => s == Seen::Response,
            None => cand.confirmed(),
        };
        let key = self.key(&cand.addr);
        let qa = self.questionable_after;
        loop {
            let idx = self.bucket_index(&cand.id);
            if let Some(updated) = self.update_member(idx, &cand, key, proven, now) {
                return updated;
            }
            if let Some(other) = self.by_addr.get(&key).copied() {
                let other_bad = self
                    .member(&other)
                    .is_some_and(|e| status_of(e, now, qa) == Status::Bad);
                if !(proven && other_bad) {
                    return false;
                }
                self.remove_member(&other);
            }
            let Some(bucket) = self.buckets.get(idx) else {
                return false;
            };
            if let Some(conflict) = bucket
                .nodes
                .iter()
                .find(|n| self.same_subnet(&n.addr, &cand.addr))
            {
                if !(proven && status_of(conflict, now, qa) == Status::Bad) {
                    return false;
                }
                let conflict_id = conflict.id;
                self.remove_member(&conflict_id);
            }
            let Some(bucket) = self.buckets.get(idx) else {
                return false;
            };
            if bucket.nodes.len() < K {
                self.insert_member(idx, cand, now);
                return true;
            }
            let bad_id = bucket
                .nodes
                .iter()
                .find(|n| status_of(n, now, qa) == Status::Bad)
                .map(|n| n.id);
            if let Some(bad_id) = bad_id {
                // Only a node that has answered us may displace a member:
                // a query-only sender waits in the replacements cache.
                if !proven {
                    self.add_replacement(idx, cand);
                    return false;
                }
                self.remove_member(&bad_id);
                self.insert_member(idx, cand, now);
                return true;
            }
            if idx == self.last_index() && self.buckets.len() < ID_BITS {
                self.split(now);
                continue;
            }
            if proven && cand.bep42 {
                // Prefer BEP 42-valid nodes: displace an invalid one, questionable first.
                let victim = bucket
                    .nodes
                    .iter()
                    .filter(|n| !n.bep42)
                    .min_by_key(|n| (status_of(n, now, qa) == Status::Good, n.last_active()))
                    .map(|n| n.id);
                if let Some(victim) = victim
                    && let Some(displaced) = self.remove_member(&victim)
                {
                    self.insert_member(idx, cand, now);
                    self.add_replacement(idx, displaced);
                    return true;
                }
            }
            self.add_replacement(idx, cand);
            return false;
        }
    }

    /// Updates an existing member with `cand.id`. `None` if there is none.
    fn update_member(
        &mut self,
        idx: usize,
        cand: &NodeEntry,
        key: AddrKey,
        proven: bool,
        now: Instant,
    ) -> Option<bool> {
        let qa = self.questionable_after;
        let bucket = self.buckets.get(idx)?;
        let entry = bucket.nodes.iter().find(|n| n.id == cand.id)?;
        if entry.addr != cand.addr {
            // The ID shows up at another endpoint: follow it only on proof, and
            // only if the old endpoint is not known to be working.
            let old_key = self.key(&entry.addr);
            let blocked = !proven
                || status_of(entry, now, qa) == Status::Good
                || (old_key != key && self.by_addr.contains_key(&key))
                || bucket
                    .nodes
                    .iter()
                    .any(|n| n.id != cand.id && self.same_subnet(&n.addr, &cand.addr));
            if blocked {
                return Some(false);
            }
            self.by_addr.remove(&old_key);
            self.by_addr.insert(key, cand.id);
        }
        let bucket = self.buckets.get_mut(idx)?;
        let entry = bucket.nodes.iter_mut().find(|n| n.id == cand.id)?;
        if entry.addr != cand.addr {
            entry.addr = cand.addr;
            entry.failures = 0;
        }
        entry.bep42 = cand.bep42;
        entry.merge(cand);
        if proven {
            bucket.last_changed = now;
        }
        Some(true)
    }

    fn insert_member(&mut self, idx: usize, entry: NodeEntry, now: Instant) {
        let key = self.key(&entry.addr);
        let policy = self.cfg.policy;
        let Some(bucket) = self.buckets.get_mut(idx) else {
            return;
        };
        bucket
            .replacements
            .retain(|r| r.id != entry.id && policy.key(&r.addr) != key);
        self.by_addr.insert(key, entry.id);
        bucket.nodes.push(entry);
        bucket.last_changed = now;
    }

    fn remove_member(&mut self, id: &NodeId) -> Option<NodeEntry> {
        let idx = self.bucket_index(id);
        let bucket = self.buckets.get_mut(idx)?;
        let pos = bucket.nodes.iter().position(|n| n.id == *id)?;
        let entry = bucket.nodes.remove(pos);
        let key = self.cfg.policy.key(&entry.addr);
        if self.by_addr.get(&key) == Some(id) {
            self.by_addr.remove(&key);
        }
        Some(entry)
    }

    fn add_replacement(&mut self, idx: usize, cand: NodeEntry) {
        let policy = self.cfg.policy;
        let key = policy.key(&cand.addr);
        let Some(bucket) = self.buckets.get_mut(idx) else {
            return;
        };
        if let Some(existing) = bucket.replacements.iter_mut().find(|r| r.id == cand.id) {
            if existing.addr == cand.addr || cand.confirmed() {
                existing.addr = cand.addr;
                existing.bep42 = cand.bep42;
                existing.merge(&cand);
            }
            return;
        }
        if let Some(pos) = bucket
            .replacements
            .iter()
            .position(|r| policy.key(&r.addr) == key)
        {
            // One candidate per address; an unproven newcomer does not evict a proven one.
            let keep_existing = bucket
                .replacements
                .get(pos)
                .is_some_and(|r| r.confirmed() && !cand.confirmed());
            if keep_existing {
                return;
            }
            bucket.replacements.remove(pos);
        }
        if bucket.replacements.len() >= REPLACEMENTS_PER_BUCKET {
            let worst = bucket
                .replacements
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.rank())
                .map(|(i, _)| i);
            match worst {
                Some(i)
                    if bucket
                        .replacements
                        .get(i)
                        .is_some_and(|w| w.rank() <= cand.rank()) =>
                {
                    bucket.replacements.remove(i);
                }
                _ => return,
            }
        }
        bucket.replacements.push(cand);
    }

    /// Moves the best eligible replacement into bucket `idx`, if it has room.
    fn promote_replacement(&mut self, idx: usize, now: Instant) -> bool {
        let Some(bucket) = self.buckets.get(idx) else {
            return false;
        };
        if bucket.nodes.len() >= K {
            return false;
        }
        let mut order: Vec<usize> = (0..bucket.replacements.len()).collect();
        order.sort_by_key(|i| std::cmp::Reverse(bucket.replacements.get(*i).map(NodeEntry::rank)));
        let chosen = order.into_iter().find(|i| {
            bucket.replacements.get(*i).is_some_and(|r| {
                self.admissible(&r.addr)
                    && !self.by_addr.contains_key(&self.key(&r.addr))
                    && !bucket
                        .nodes
                        .iter()
                        .any(|n| self.same_subnet(&n.addr, &r.addr))
            })
        });
        let Some(pos) = chosen else { return false };
        let Some(bucket) = self.buckets.get_mut(idx) else {
            return false;
        };
        if pos >= bucket.replacements.len() {
            return false;
        }
        let entry = bucket.replacements.remove(pos);
        self.insert_member(idx, entry, now);
        true
    }

    fn fill_from_replacements(&mut self, idx: usize, now: Instant) {
        while self.promote_replacement(idx, now) {}
    }

    /// Splits the last bucket (the one holding our own ID) in two.
    fn split(&mut self, now: Instant) {
        let old_idx = self.last_index();
        let own = self.own_id;
        let Some(old) = self.buckets.get_mut(old_idx) else {
            return;
        };
        let (stay, go): (Vec<_>, Vec<_>) = std::mem::take(&mut old.nodes)
            .into_iter()
            .partition(|n| own.common_prefix_len(&n.id) == old_idx);
        let (stay_r, go_r): (Vec<_>, Vec<_>) = std::mem::take(&mut old.replacements)
            .into_iter()
            .partition(|n| own.common_prefix_len(&n.id) == old_idx);
        old.nodes = stay;
        old.replacements = stay_r;
        let last_changed = old.last_changed;
        self.buckets.push(Bucket {
            nodes: go,
            replacements: go_r,
            last_changed,
        });
        self.fill_from_replacements(old_idx, now);
        self.fill_from_replacements(old_idx.saturating_add(1), now);
    }

    /// A query to `addr` failed.
    pub(crate) fn on_failure(&mut self, addr: &SocketAddr, now: Instant) {
        let qa = self.questionable_after;
        let Some(id) = self.by_addr.get(&self.key(addr)).copied() else {
            return;
        };
        let Some(entry) = self.member_mut(&id) else {
            return;
        };
        if entry.addr != *addr {
            return;
        }
        entry.failures = entry.failures.saturating_add(1);
        if status_of(entry, now, qa) != Status::Bad {
            return;
        }
        let failures = entry.failures;
        let idx = self.bucket_index(&id);
        let has_replacement = self
            .buckets
            .get(idx)
            .is_some_and(|b| !b.replacements.is_empty());
        if has_replacement || failures >= FORGET_AFTER_FAILURES {
            self.remove_member(&id);
            self.fill_from_replacements(idx, now);
        }
    }

    /// A query to `addr` that expected `expected` was answered under another
    /// ID. That counts as a failure of `expected` if it is the member at
    /// `addr`, so entries whose endpoint now belongs to another node age out.
    pub(crate) fn on_wrong_id(&mut self, expected: &NodeId, addr: &SocketAddr, now: Instant) {
        if self.by_addr.get(&self.key(addr)) == Some(expected) {
            self.on_failure(addr, now);
        }
    }

    /// Up to `count` non-bad members closest to `target`, closest first.
    /// With `confirmed_only`, nodes that never answered us are skipped.
    pub(crate) fn closest(
        &self,
        target: &NodeId,
        count: usize,
        now: Instant,
        confirmed_only: bool,
    ) -> Vec<CompactNode> {
        let mut all: Vec<(Distance, CompactNode)> = self
            .members()
            .filter(|e| (!confirmed_only || e.confirmed()) && self.status(e, now) != Status::Bad)
            .map(|e| (e.id.distance(target), e.node()))
            .collect();
        if count > 0 && all.len() > count {
            all.select_nth_unstable_by_key(count.saturating_sub(1), |(d, _)| *d);
            all.truncate(count);
        }
        all.sort_unstable_by_key(|(d, _)| *d);
        all.truncate(count);
        all.into_iter().map(|(_, n)| n).collect()
    }

    /// Random lookup targets for at most `max` buckets unchanged for
    /// `interval`, least recently changed first. Only those buckets count as
    /// refreshed from now on; the others stay due.
    pub(crate) fn refresh_targets(
        &mut self,
        now: Instant,
        interval: Duration,
        max: usize,
    ) -> Vec<NodeId> {
        let last = self.last_index();
        let own = self.own_id;
        let mut due: Vec<(Instant, usize)> = self
            .buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| now.saturating_duration_since(b.last_changed) >= interval)
            .map(|(i, b)| (b.last_changed, i))
            .collect();
        due.sort_unstable();
        due.truncate(max);
        let mut out = Vec::with_capacity(due.len());
        for (_, i) in due {
            if let Some(bucket) = self.buckets.get_mut(i) {
                bucket.last_changed = now;
                out.push(own.random_with_prefix(i, i < last));
            }
        }
        out
    }

    /// Up to `max` members that are not good and were not pinged within
    /// `min_interval`, least recently active first. They are marked as pinged.
    pub(crate) fn ping_candidates(
        &mut self,
        now: Instant,
        min_interval: Duration,
        max: usize,
    ) -> Vec<CompactNode> {
        let qa = self.questionable_after;
        let mut cands: Vec<(Option<Instant>, usize, usize)> = Vec::new();
        for (bi, bucket) in self.buckets.iter().enumerate() {
            for (ni, n) in bucket.nodes.iter().enumerate() {
                let due = n
                    .last_ping
                    .is_none_or(|t| now.saturating_duration_since(t) >= min_interval);
                if due && status_of(n, now, qa) != Status::Good {
                    cands.push((n.last_active(), bi, ni));
                }
            }
        }
        cands.sort_unstable();
        let mut out = Vec::new();
        for (_, bi, ni) in cands.into_iter().take(max) {
            if let Some(n) = self.buckets.get_mut(bi).and_then(|b| b.nodes.get_mut(ni)) {
                n.last_ping = Some(now);
                out.push(n.node());
            }
        }
        out
    }

    /// Up to `max` confirmed, non-bad members for persistence: good nodes
    /// first, then by most recent answer.
    pub(crate) fn export(&self, max: usize, now: Instant) -> Vec<CompactNode> {
        let mut nodes: Vec<&NodeEntry> = self
            .members()
            .filter(|e| e.confirmed() && self.status(e, now) != Status::Bad)
            .collect();
        nodes.sort_by_key(|e| {
            (
                std::cmp::Reverse(self.status(e, now) == Status::Good),
                std::cmp::Reverse(e.last_response),
            )
        });
        nodes.into_iter().take(max).map(NodeEntry::node).collect()
    }

    /// The same nodes organised around a new own ID.
    pub(crate) fn rebuild(&self, new_id: NodeId, now: Instant) -> RoutingTable {
        let mut table = RoutingTable::new(new_id, now, self.cfg.clone());
        for e in self.members() {
            table.upsert(e.clone(), None, now);
        }
        for e in self.buckets.iter().flat_map(|b| b.replacements.iter()) {
            table.upsert(e.clone(), None, now);
        }
        table
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const QA: Duration = Duration::from_secs(15 * 60);
    const PRODUCTION: AddrPolicy = AddrPolicy {
        allow_private: false,
        by_endpoint: false,
    };
    const LOCAL_TEST: AddrPolicy = AddrPolicy {
        allow_private: true,
        by_endpoint: true,
    };

    fn config(family: Family, policy: AddrPolicy) -> TableConfig {
        TableConfig {
            family,
            questionable_after: QA,
            policy,
            own: Arc::new(OwnAddrs::default()),
        }
    }

    fn table(now: Instant) -> RoutingTable {
        RoutingTable::new(NodeId([0x55; 20]), now, config(Family::V4, PRODUCTION))
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// A public endpoint in its own /24.
    fn addr(i: u32) -> SocketAddr {
        SocketAddr::from(([8, (i >> 8) as u8, i as u8, 1], 6881))
    }

    fn check(t: &RoutingTable) {
        let members: Vec<&NodeEntry> = t.members().collect();
        assert_eq!(members.len(), t.by_addr.len());
        let mut ids = HashSet::new();
        for (i, b) in t.buckets.iter().enumerate() {
            assert!(b.nodes.len() <= K);
            assert!(b.replacements.len() <= REPLACEMENTS_PER_BUCKET);
            for n in &b.nodes {
                assert!(ids.insert(n.id));
                assert_eq!(t.bucket_index(&n.id), i);
                assert_eq!(t.by_addr[&t.key(&n.addr)], n.id);
                assert_ne!(n.id, t.own_id);
            }
            for (x, a) in b.nodes.iter().enumerate() {
                for c in b.nodes.iter().skip(x + 1) {
                    assert!(!t.same_subnet(&a.addr, &c.addr));
                }
            }
        }
    }

    #[test]
    fn insert_and_closest() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        let mut ids = Vec::new();
        for i in 0..40u32 {
            let id = own.random_with_prefix((i % 20) as usize, true);
            ids.push(id);
            t.on_response(id, addr(i), true, now);
            check(&t);
        }
        assert!(t.len() > 8);
        assert!(t.num_buckets() > 1);
        let target = NodeId::random();
        let got = t.closest(&target, 8, now, true);
        assert_eq!(got.len(), 8);
        for w in got.windows(2) {
            assert!(w[0].id.distance(&target) <= w[1].id.distance(&target));
        }
        // Nothing outside the result is closer than its farthest entry.
        let worst = got.last().unwrap().id.distance(&target);
        let chosen: HashSet<_> = got.iter().map(|n| n.id).collect();
        for e in t.members() {
            if !chosen.contains(&e.id) {
                assert!(e.id.distance(&target) >= worst);
            }
        }
        assert!(t.closest(&target, 0, now, true).is_empty());
    }

    #[test]
    fn only_own_bucket_splits() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        // 20 far nodes (common prefix 0) — they all belong to bucket 0.
        // (`true` simulates BEP 42-valid IDs; this test is about splits.)
        for i in 0..20u32 {
            t.on_response(own.random_with_prefix(0, true), addr(i), true, now);
        }
        check(&t);
        // The first split happened (bucket 0 held our own ID), but bucket 0
        // can never split again: it stays at K, the rest wait as replacements.
        assert_eq!(t.num_buckets(), 2);
        assert_eq!(t.len(), K);
        assert_eq!(t.buckets[0].replacements.len(), REPLACEMENTS_PER_BUCKET);
        // Near nodes keep splitting the last bucket.
        // (`true` simulates BEP 42-valid IDs; this test is about splits.)
        for i in 0..30u32 {
            t.on_response(
                own.random_with_prefix(10 + (i as usize % 10), true),
                addr(100 + i),
                true,
                now,
            );
            check(&t);
        }
        assert!(t.num_buckets() > 10);
        assert_eq!(t.len(), K + 30);
    }

    #[test]
    fn ignores_self_and_undialable() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        assert!(!t.on_response(own, addr(1), true, now));
        let bad = [
            "127.0.0.1:1",
            "10.0.0.1:1",
            "8.8.8.8:0",
            "0.0.0.0:5",
            "[2a00::1]:5",
            "[fd00::1]:5",
        ];
        for bad in bad {
            assert!(
                !t.on_response(NodeId::random(), sa(bad), true, now),
                "{bad}"
            );
        }
        assert_eq!(t.len(), 0);
        // An IPv6 table takes neither IPv4 nor IPv4-mapped addresses.
        let mut t6 = RoutingTable::new(own, now, config(Family::V6, PRODUCTION));
        assert!(!t6.on_response(NodeId::random(), sa("8.8.8.8:1"), true, now));
        assert!(!t6.on_response(NodeId::random(), sa("[::ffff:8.8.8.8]:1"), true, now));
        assert!(t6.on_response(NodeId::random(), sa("[2a00::1]:1"), true, now));
        // Tests: loopback endpoints are distinct nodes.
        let mut local = RoutingTable::new(own, now, config(Family::V4, LOCAL_TEST));
        assert!(local.on_response(NodeId::random(), sa("127.0.0.1:1"), true, now));
        assert!(local.on_response(NodeId::random(), sa("127.0.0.1:2"), true, now));
        assert!(!local.on_response(NodeId::random(), sa("127.0.0.1:0"), true, now));
        assert!(!local.on_response(NodeId::random(), sa("0.0.0.0:3"), true, now));
        assert_eq!(local.len(), 2);
    }

    #[test]
    fn production_keys_private_addresses_on_ip() {
        // `allow_private_addrs` alone does not relax the per-IP rules.
        let now = Instant::now();
        let policy = AddrPolicy {
            allow_private: true,
            by_endpoint: false,
        };
        let mut t = RoutingTable::new(NodeId([0x55; 20]), now, config(Family::V4, policy));
        let own = t.own_id();
        assert!(t.on_response(
            own.random_with_prefix(0, true),
            sa("127.0.0.1:1"),
            true,
            now
        ));
        // Same IP, other port, other bucket: rejected.
        assert!(!t.on_response(
            own.random_with_prefix(3, true),
            sa("127.0.0.1:2"),
            true,
            now
        ));
        // Same /24, same bucket: rejected.
        assert!(!t.on_response(
            own.random_with_prefix(0, true),
            sa("127.0.0.2:1"),
            true,
            now
        ));
        assert_eq!(t.len(), 1);
        check(&t);
    }

    #[test]
    fn own_addresses_are_never_members() {
        let now = Instant::now();
        let mut ours = OwnAddrs::default();
        ours.add(sa("8.8.8.8:6881"));
        let cfg = TableConfig {
            own: Arc::new(ours),
            ..config(Family::V4, PRODUCTION)
        };
        let mut t = RoutingTable::new(NodeId([0x55; 20]), now, cfg);
        let own = t.own_id();
        // Any port of our own IP is refused in production.
        assert!(!t.on_response(
            own.random_with_prefix(0, true),
            sa("8.8.8.8:6881"),
            true,
            now
        ));
        assert!(!t.on_response(own.random_with_prefix(0, true), sa("8.8.8.8:1"), true, now));
        assert!(t.on_response(own.random_with_prefix(0, true), sa("9.9.9.9:1"), true, now));
        // Learning a new own address evicts the member using it and promotes a replacement.
        for i in 0..K as u32 {
            assert!(t.on_response(
                own.random_with_prefix(1 + i as usize, true),
                addr(i),
                true,
                now
            ));
        }
        let far: Vec<NodeId> = (0..K as u32 + 1)
            .map(|i| {
                let id = own.random_with_prefix(0, true);
                t.on_response(id, addr(100 + i), true, now);
                id
            })
            .collect();
        // Bucket 0: 9.9.9.9 and far[0..7]; far[7] and far[8] wait as replacements.
        assert_eq!(t.buckets[0].nodes.len(), K);
        assert_eq!(t.buckets[0].replacements.len(), 2);
        let before = t.len();
        // Now 9.9.9.9, far[0] (a member) and far[8] (a replacement) turn out to be ours.
        let mut ours = OwnAddrs::default();
        ours.add(sa("9.9.9.9:6881"));
        ours.add(addr(100));
        ours.add(addr(108));
        t.set_own(Arc::new(ours), now);
        assert!(
            t.members()
                .all(|e| e.addr != sa("9.9.9.9:1") && e.addr != addr(100))
        );
        // Only far[7] could replace the two members that left.
        assert!(t.buckets[0].replacements.is_empty());
        assert_eq!(t.len(), before - 1);
        let present: Vec<bool> = far.iter().map(|id| t.member(id).is_some()).collect();
        assert_eq!(
            present,
            [false, true, true, true, true, true, true, true, false]
        );
        check(&t);
        // The table remembers the new addresses.
        assert!(!t.on_response(own.random_with_prefix(0, true), sa("9.9.9.9:2"), true, now));
    }

    #[test]
    fn one_entry_per_ip_and_subnet() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        let a = own.random_with_prefix(0, true);
        assert!(t.on_response(a, "8.8.8.8:1".parse().unwrap(), true, now));
        // Same IP, other ID and port: rejected even in another bucket.
        let b = own.random_with_prefix(3, true);
        assert!(!t.on_response(b, "8.8.8.8:2".parse().unwrap(), true, now));
        // Same /24 in the same bucket: rejected.
        let c = own.random_with_prefix(0, true);
        assert!(!t.on_response(c, "8.8.8.9:1".parse().unwrap(), true, now));
        // Split the table so near nodes live in other buckets than `a`.
        for i in 0..K {
            assert!(t.on_response(
                own.random_with_prefix(2 + i, true),
                addr(i as u32),
                true,
                now
            ));
        }
        assert!(t.num_buckets() > 1);
        // Same /24 as `a`, but in another bucket: accepted.
        assert!(t.on_response(b, "8.8.8.10:1".parse().unwrap(), true, now));
        assert_ne!(t.bucket_index(&a), t.bucket_index(&b));
        check(&t);
        assert_eq!(t.len(), K + 2);
        // IPv6: same /64 in one bucket conflicts.
        let mut t6 = RoutingTable::new(own, now, config(Family::V6, PRODUCTION));
        assert!(t6.on_response(a, "[2a00:1:2:3::1]:1".parse().unwrap(), true, now));
        assert!(!t6.on_response(c, "[2a00:1:2:3::2]:1".parse().unwrap(), true, now));
        assert!(t6.on_response(c, "[2a00:1:2:4::2]:1".parse().unwrap(), true, now));
    }

    #[test]
    fn status_transitions() {
        let t0 = Instant::now();
        let mut t = table(t0);
        let own = t.own_id();
        let id = own.random_with_prefix(0, true);
        let a = addr(1);
        t.on_response(id, a, true, t0);
        let status = |t: &RoutingTable, now| t.status(t.member(&id).unwrap(), now);
        assert_eq!(status(&t, t0), Status::Good);
        let later = t0 + QA;
        assert_eq!(status(&t, later), Status::Questionable);
        // An inbound query from a node that answered before makes it good again.
        t.on_query(id, a, true, later);
        assert_eq!(status(&t, later), Status::Good);
        // Failures: questionable, then bad after MAX_FAILURES.
        t.on_failure(&a, later);
        assert_eq!(status(&t, later), Status::Questionable);
        t.on_failure(&a, later);
        t.on_failure(&a, later);
        assert_eq!(status(&t, later), Status::Bad);
        assert!(t.closest(&id, 8, later, false).is_empty());
        // A response clears the failures.
        t.on_response(id, a, true, later);
        assert_eq!(status(&t, later), Status::Good);
        // A failure from another port of the same IP is not this node's.
        t.on_failure(&"8.0.1.1:9".parse().unwrap(), later);
        assert_eq!(t.member(&id).unwrap().failures, 0);
    }

    #[test]
    fn unconfirmed_nodes() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        let id = own.random_with_prefix(0, true);
        let a = addr(1);
        assert!(t.on_query(id, a, true, now));
        assert_eq!(t.status(t.member(&id).unwrap(), now), Status::Questionable);
        assert!(t.closest(&id, 8, now, true).is_empty());
        assert_eq!(t.closest(&id, 8, now, false).len(), 1);
        assert_eq!(
            t.ping_candidates(now, Duration::from_secs(60), 10),
            vec![CompactNode { id, addr: a }]
        );
        // Already pinged: not offered again until the interval passes.
        assert!(
            t.ping_candidates(now, Duration::from_secs(60), 10)
                .is_empty()
        );
        assert_eq!(
            t.ping_candidates(now + Duration::from_secs(60), Duration::from_secs(60), 10)
                .len(),
            1
        );
        // One failure makes it bad; with nothing to replace it, it stays until forgotten.
        t.on_failure(&a, now);
        assert_eq!(t.status(t.member(&id).unwrap(), now), Status::Bad);
        for _ in 1..FORGET_AFTER_FAILURES {
            t.on_failure(&a, now);
        }
        assert_eq!(t.len(), 0);
        check(&t);
    }

    #[test]
    fn bad_nodes_are_replaced_from_cache() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        // Force a split first so bucket 0 is a non-splittable bucket.
        t.on_response(own.random_with_prefix(5, true), addr(999), true, now);
        let mut members = Vec::new();
        for i in 0..K as u32 {
            let id = own.random_with_prefix(0, true);
            t.on_response(id, addr(i), true, now);
            members.push((id, addr(i)));
        }
        assert_eq!(t.buckets[0].nodes.len(), K);
        let extra_unconfirmed = own.random_with_prefix(0, true);
        t.on_query(extra_unconfirmed, addr(50), true, now);
        let extra_invalid = own.random_with_prefix(0, true);
        t.on_response(extra_invalid, addr(51), false, now);
        let extra_best = own.random_with_prefix(0, true);
        // A valid BEP 42 node cannot displace valid good members; it waits.
        assert!(!t.on_response(extra_best, addr(52), true, now));
        assert_eq!(t.buckets[0].replacements.len(), 3);
        check(&t);
        // A member goes bad: the best replacement (confirmed, BEP 42) takes its place.
        let (bad_id, bad_addr) = members[3];
        for _ in 0..MAX_FAILURES {
            t.on_failure(&bad_addr, now);
        }
        assert!(t.member(&bad_id).is_none());
        assert!(t.member(&extra_best).is_some());
        check(&t);
        // A query-only node cannot take a bad member's slot directly: it
        // waits in the replacements cache until it answers us.
        let (bad2, bad2_addr) = members[4];
        t.buckets[0].replacements.clear();
        for _ in 0..MAX_FAILURES {
            t.on_failure(&bad2_addr, now);
        }
        assert!(t.member(&bad2).is_some());
        let newcomer = own.random_with_prefix(0, true);
        assert!(!t.on_query(newcomer, addr(60), true, now));
        assert!(t.member(&bad2).is_some());
        // Once it answers, it is proven and may displace the bad member.
        assert!(t.on_response(newcomer, addr(60), true, now));
        assert!(t.member(&bad2).is_none());
        check(&t);
    }

    #[test]
    fn bep42_invalid_public_senders_wait_in_replacements() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        let mut invalid = Vec::new();
        for i in 0..K as u32 {
            let id = own.random_with_prefix(0, true);
            // Invalid IDs from public senders never become members, even
            // when they answer us: they wait in the replacements cache.
            assert!(!t.on_response(id, addr(i), false, now));
            assert!(t.member(&id).is_none());
            invalid.push(id);
        }
        assert_eq!(t.len(), 0);
        // Fill the bucket with proven, valid members.
        for i in 0..K as u32 {
            let id = own.random_with_prefix(0, true);
            assert!(t.on_response(id, addr(100 + i), true, now));
        }
        assert_eq!(t.len(), K);
        // A query-only node cannot displace a member ...
        let newcomer = own.random_with_prefix(0, true);
        assert!(!t.on_query(newcomer, addr(200), true, now));
        assert!(t.member(&newcomer).is_none());
        // ... nor can a proven node whose ID is invalid for its address.
        let spoof = own.random_with_prefix(0, true);
        assert!(!t.on_response(spoof, addr(201), false, now));
        assert!(t.member(&spoof).is_none());
        for id in &invalid {
            assert!(t.member(id).is_none());
        }
        check(&t);
    }

    #[test]
    fn node_moving_address() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        let id = own.random_with_prefix(0, true);
        t.on_response(id, addr(1), true, now);
        // A good node does not move on hearsay or even a response.
        assert!(!t.on_query(id, addr(2), true, now));
        assert!(!t.on_response(id, addr(2), true, now));
        assert_eq!(t.member(&id).unwrap().addr, addr(1));
        // Once questionable, a response from the new endpoint moves it.
        let later = now + QA;
        assert!(t.on_response(id, addr(2), true, later));
        assert_eq!(t.member(&id).unwrap().addr, addr(2));
        check(&t);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn refresh_targets_cover_buckets() {
        let t0 = Instant::now();
        let mut t = table(t0);
        let own = t.own_id();
        for i in 0..30u32 {
            t.on_response(
                own.random_with_prefix((i % 6) as usize, true),
                addr(i),
                true,
                t0,
            );
        }
        let n = t.num_buckets();
        assert!(
            t.refresh_targets(t0 + Duration::from_secs(60), QA, usize::MAX)
                .is_empty()
        );
        let targets = t.refresh_targets(t0 + QA, QA, usize::MAX);
        assert_eq!(targets.len(), n);
        for (i, target) in targets.iter().enumerate() {
            assert_eq!(t.bucket_index(target), i);
        }
        assert!(t.refresh_targets(t0 + QA, QA, usize::MAX).is_empty());
    }

    #[test]
    fn limited_refreshes_reach_every_bucket() {
        let t0 = Instant::now();
        let mut t = table(t0);
        let own = t.own_id();
        for i in 0..30u32 {
            t.on_response(
                own.random_with_prefix((i % 6) as usize, true),
                addr(i),
                true,
                t0,
            );
        }
        let n = t.num_buckets();
        assert!(n > 2);
        // Two lookups at a time: the buckets that were not picked stay due.
        let mut refreshed = HashSet::new();
        for round in 0..n {
            let now = t0 + QA + Duration::from_secs(round as u64);
            let targets = t.refresh_targets(now, QA, 2);
            assert!(targets.len() <= 2);
            refreshed.extend(targets.iter().map(|x| t.bucket_index(x)));
        }
        assert_eq!(refreshed.len(), n);
    }

    #[test]
    fn wrong_id_replies_retire_the_entry() {
        let t0 = Instant::now();
        let mut t = table(t0);
        let own = t.own_id();
        let e = addr(1);
        let squatter = own.random_with_prefix(0, true);
        let real = own.random_with_prefix(0, true);
        // An unconfirmed entry answered under another ID is replaced at once.
        assert!(t.on_query(squatter, e, true, t0));
        assert!(!t.on_response(real, e, true, t0));
        t.on_wrong_id(&squatter, &e, t0);
        assert!(t.on_response(real, e, true, t0));
        assert!(t.member(&squatter).is_none());
        check(&t);
        // A confirmed entry needs as many failures as a timeout would.
        let t1 = t0 + QA;
        let newer = own.random_with_prefix(0, true);
        for _ in 0..MAX_FAILURES - 1 {
            t.on_wrong_id(&real, &e, t1);
            assert!(!t.on_response(newer, e, true, t1));
        }
        t.on_wrong_id(&real, &e, t1);
        assert!(t.on_response(newer, e, true, t1));
        assert!(t.member(&real).is_none());
        // Only the entry at that endpoint is charged.
        let other = own.random_with_prefix(1, true);
        assert!(t.on_response(other, addr(2), true, t1));
        t.on_wrong_id(&other, &e, t1);
        t.on_wrong_id(&newer, &addr(2), t1);
        assert!(t.members().all(|m| m.failures == 0));
        check(&t);
    }

    #[test]
    fn export_and_rebuild() {
        let now = Instant::now();
        let mut t = table(now);
        let own = t.own_id();
        for i in 0..30u32 {
            t.on_response(
                own.random_with_prefix((i % 8) as usize, true),
                addr(i),
                true,
                now,
            );
        }
        t.on_query(own.random_with_prefix(0, true), addr(100), true, now);
        let exported = t.export(300, now);
        assert_eq!(
            exported.len(),
            t.members().filter(|e| e.confirmed()).count()
        );
        assert_eq!(t.export(5, now).len(), 5);
        let new_id = NodeId::random();
        let rebuilt = t.rebuild(new_id, now);
        assert_eq!(rebuilt.own_id(), new_id);
        check(&rebuilt);
        assert!(rebuilt.len() >= K.min(t.len()));
        for e in rebuilt.members() {
            let old = t.members().find(|o| o.id == e.id).map(|o| o.last_response);
            let replacement = t
                .buckets
                .iter()
                .flat_map(|b| b.replacements.iter())
                .find(|o| o.id == e.id);
            assert!(old.is_some() || replacement.is_some());
        }
    }
}
