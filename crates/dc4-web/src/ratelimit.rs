//! In-memory rate limits per client prefix (`docs/03-design.md` §3, §12).
//!
//! Each bucket is a GCRA token bucket: `burst` requests may arrive at once,
//! and one more is allowed every `interval`. IPv4 clients are keyed on their
//! /32. IPv6 clients are keyed on their /64, /56, /48 and /32 at the same
//! time, and a request must be allowed by every one of those buckets;
//! nothing is charged unless all of them allow it.
//!
//! The class quota applies to an IPv4 /32 and to an IPv6 /64. A wider IPv6
//! prefix contains many /64s, so its bucket is [`V6_QUOTA_MULTIPLIERS`]
//! times larger (in rate and in burst). With equal buckets, every request
//! that charges a /64 would also charge its /48, and the /64 and /56 buckets
//! could never refuse anything on their own. The /32 bucket bounds how many
//! keys one IPv6 allocation can keep in the map.
//!
//! The key map is bounded. Entries idle for [`RATE_LIMIT_IDLE_EXPIRY`] are
//! removed. While the map is full, entries whose bucket has refilled are
//! removed too: such a bucket behaves exactly like a new one, so forgetting
//! it changes nothing. Live entries are never evicted: while the map is
//! still full, a new client is charged to one of [`OVERFLOW_SHARDS`]
//! overflow buckets per class, with the class quota, chosen by a keyed hash
//! of its IPv4 /16 or IPv6 /32. One network that fills the map can then
//! drain only its own overflow bucket. The state lives only in memory
//! (R11).

use std::cmp::max;
use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Most client prefixes tracked at once, over all classes.
pub const RATE_LIMIT_MAX_KEYS: usize = 100_000;
/// A prefix not seen for this long is forgotten.
pub const RATE_LIMIT_IDLE_EXPIRY: Duration = Duration::from_secs(10 * 60);
/// Quota for pages, static files and health checks: 3 per second, burst 30.
pub const PAGE_QUOTA: Quota = Quota::per_second(3, 30);
/// Quota for the JSON API: 2 per second, burst 20.
pub const API_QUOTA: Quota = Quota::per_second(2, 20);
/// IPv6 prefix lengths a client is limited on, with the factor applied to
/// the class quota for each.
pub const V6_QUOTA_MULTIPLIERS: [(u8, u32); 4] = [(64, 1), (56, 4), (48, 16), (32, 64)];
/// Overflow buckets per class while the key map is full.
pub const OVERFLOW_SHARDS: usize = 64;

/// Idle entries are removed at least this often.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// While the map is full, a new client triggers a sweep at most this often.
const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const NANOS_PER_MINUTE: u64 = 60 * NANOS_PER_SECOND;

/// Prefixes per request at most (the IPv6 case).
const MAX_PREFIXES: usize = V6_QUOTA_MULTIPLIERS.len();

/// A rate: `burst` requests at once, then one per `interval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// Time between two requests once the burst is used up.
    pub interval: Duration,
    /// Requests allowed at once.
    pub burst: u32,
}

impl Quota {
    /// `count` requests per second with the given burst.
    pub const fn per_second(count: u64, burst: u32) -> Quota {
        Quota::spread(NANOS_PER_SECOND, count, burst)
    }

    /// `count` requests per minute with the given burst.
    pub const fn per_minute(count: u64, burst: u32) -> Quota {
        Quota::spread(NANOS_PER_MINUTE, count, burst)
    }

    const fn spread(period_nanos: u64, count: u64, burst: u32) -> Quota {
        let interval = match period_nanos.checked_div(count) {
            Some(n) => n,
            None => period_nanos,
        };
        Quota {
            interval: Duration::from_nanos(interval),
            burst,
        }
    }

    /// This quota `factor` times over: `factor` times the rate and burst.
    fn scaled(self, factor: u32) -> Quota {
        Quota {
            interval: self.interval.checked_div(factor).unwrap_or(self.interval),
            burst: self.burst.saturating_mul(factor),
        }
    }

    /// How far ahead of now a bucket's schedule may run and still admit a
    /// request.
    fn tolerance(&self) -> Duration {
        self.interval.saturating_mul(self.burst.saturating_sub(1))
    }
}

/// Request classes with separate quotas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RateClass {
    /// Pages, static files, health checks and unknown paths.
    Page,
    /// The JSON API.
    Api,
}

impl RateClass {
    fn quota(self) -> Quota {
        match self {
            RateClass::Page => PAGE_QUOTA,
            RateClass::Api => API_QUOTA,
        }
    }
}

/// The outcome of [`RateLimiter::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    /// Refused; a request would be allowed after this long.
    Deny(Duration),
}

/// A client network: an IPv4 /32 or an IPv6 prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Prefix {
    V4(u32),
    V6 { bits: u128, len: u8 },
}

impl Prefix {
    /// The quota of this prefix's bucket in `class`.
    fn quota(self, class: RateClass) -> Quota {
        let base = class.quota();
        match self {
            Prefix::V4(_) => base,
            Prefix::V6 { len, .. } => V6_QUOTA_MULTIPLIERS
                .iter()
                .find(|(l, _)| *l == len)
                .map_or(base, |(_, factor)| base.scaled(*factor)),
        }
    }
}

type Key = (RateClass, Prefix);

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Theoretical arrival time of the next request (GCRA).
    tat: Instant,
    /// Last request, allowed or not.
    seen: Instant,
}

/// Where one prefix of a request is counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Existing(Key),
    New(Key),
    /// The overflow bucket of the request's class and shard.
    Overflow,
}

/// An overflow bucket: a class and a shard below [`OVERFLOW_SHARDS`].
type OverflowKey = (RateClass, usize);

struct State {
    buckets: HashMap<Key, Bucket>,
    overflow: HashMap<OverflowKey, Bucket>,
    last_sweep: Option<Instant>,
}

/// Bounded, multi-prefix rate limiter.
pub(crate) struct RateLimiter {
    state: Mutex<State>,
    capacity: usize,
    idle: Duration,
    /// Keys the overflow shard choice, so clients cannot pick a shard.
    shard_hasher: RandomState,
}

impl RateLimiter {
    /// A limiter tracking at most `capacity` prefixes, forgetting a prefix
    /// after `idle` without requests.
    pub(crate) fn new(capacity: usize, idle: Duration) -> RateLimiter {
        RateLimiter {
            state: Mutex::new(State {
                buckets: HashMap::new(),
                overflow: HashMap::new(),
                last_sweep: None,
            }),
            capacity,
            idle,
            shard_hasher: RandomState::new(),
        }
    }

    /// The overflow shard of `ip`: a keyed hash of its IPv4 /16 or IPv6 /32.
    fn shard_of(&self, ip: IpAddr) -> usize {
        let network = match ip.to_canonical() {
            IpAddr::V4(v4) => (4u8, u128::from(u32::from(v4) >> 16)),
            IpAddr::V6(v6) => (6u8, u128::from(v6) >> 96),
        };
        let hash = self.shard_hasher.hash_one(network);
        let shards = u64::try_from(OVERFLOW_SHARDS).unwrap_or(u64::MAX);
        // The remainder is below OVERFLOW_SHARDS, so it fits in usize.
        hash.checked_rem(shards)
            .and_then(|shard| usize::try_from(shard).ok())
            .unwrap_or(0)
    }

    /// Charges one request of `class` from `ip` at `now`, if every bucket
    /// that applies allows it.
    pub(crate) fn check(&self, class: RateClass, ip: IpAddr, now: Instant) -> Verdict {
        let (prefixes, count) = prefixes_of(ip);
        let prefixes = prefixes.get(..count).unwrap_or_default();
        let overflow_key = (class, self.shard_of(ip));
        // The lock only guards plain data, which stays consistent even if a
        // holder panicked.
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        let missing = prefixes
            .iter()
            .filter(|p| !st.buckets.contains_key(&(class, **p)))
            .count();
        let full = st.buckets.len().saturating_add(missing) > self.capacity;
        st.sweep_if_due(now, self.idle, full);

        let mut slots = [Slot::Overflow; MAX_PREFIXES];
        let mut added = 0usize;
        for (slot, prefix) in slots.iter_mut().zip(prefixes) {
            let key = (class, *prefix);
            *slot = if st.buckets.contains_key(&key) {
                Slot::Existing(key)
            } else if st.buckets.len().saturating_add(added) < self.capacity {
                added = added.saturating_add(1);
                Slot::New(key)
            } else {
                Slot::Overflow
            };
        }
        let slots = slots.get(..prefixes.len()).unwrap_or_default();

        let mut wait = Duration::ZERO;
        for slot in slots {
            let tat = st.tat(*slot, overflow_key).unwrap_or(now);
            wait = max(wait, delay(tat, now, slot_quota(*slot, class)));
        }

        if !wait.is_zero() {
            for slot in slots {
                if let Slot::Existing(key) = slot
                    && let Some(bucket) = st.buckets.get_mut(key)
                {
                    bucket.seen = now;
                }
            }
            return Verdict::Deny(wait);
        }

        let mut overflow_charged = false;
        for slot in slots {
            let quota = slot_quota(*slot, class);
            match slot {
                Slot::Existing(key) => {
                    if let Some(bucket) = st.buckets.get_mut(key) {
                        charge(bucket, now, quota);
                    }
                }
                Slot::New(key) => {
                    let mut bucket = Bucket {
                        tat: now,
                        seen: now,
                    };
                    charge(&mut bucket, now, quota);
                    st.buckets.insert(*key, bucket);
                }
                Slot::Overflow => {
                    if !overflow_charged {
                        overflow_charged = true;
                        let bucket = st.overflow.entry(overflow_key).or_insert(Bucket {
                            tat: now,
                            seen: now,
                        });
                        charge(bucket, now, quota);
                    }
                }
            }
        }
        Verdict::Allow
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .buckets
            .len()
    }
}

impl State {
    fn tat(&self, slot: Slot, overflow: OverflowKey) -> Option<Instant> {
        match slot {
            Slot::Existing(key) => self.buckets.get(&key).map(|b| b.tat),
            Slot::New(_) => None,
            Slot::Overflow => self.overflow.get(&overflow).map(|b| b.tat),
        }
    }

    /// Removes idle entries every [`SWEEP_INTERVAL`]. While the map is
    /// `full`, it does so every [`FULL_SWEEP_INTERVAL`] and also removes
    /// entries whose bucket has refilled.
    fn sweep_if_due(&mut self, now: Instant, idle: Duration, full: bool) {
        let every = if full {
            FULL_SWEEP_INTERVAL
        } else {
            SWEEP_INTERVAL
        };
        let due = match self.last_sweep {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= every,
        };
        if !due {
            return;
        }
        self.last_sweep = Some(now);
        self.buckets
            .retain(|_, b| now.saturating_duration_since(b.seen) < idle && (!full || b.tat > now));
    }
}

/// The quota of the bucket behind `slot`. Overflow buckets have the class
/// quota.
fn slot_quota(slot: Slot, class: RateClass) -> Quota {
    match slot {
        Slot::Existing((_, prefix)) | Slot::New((_, prefix)) => prefix.quota(class),
        Slot::Overflow => class.quota(),
    }
}

/// Time until a bucket whose schedule is at `tat` admits a request.
fn delay(tat: Instant, now: Instant, quota: Quota) -> Duration {
    tat.saturating_duration_since(now)
        .saturating_sub(quota.tolerance())
}

/// Records one admitted request.
fn charge(bucket: &mut Bucket, now: Instant, quota: Quota) {
    let start = max(bucket.tat, now);
    // An `Instant` this far in the future cannot occur; keeping `start`
    // merely allows one extra request.
    bucket.tat = start.checked_add(quota.interval).unwrap_or(start);
    bucket.seen = now;
}

/// The prefixes `ip` is limited on, and how many there are.
fn prefixes_of(ip: IpAddr) -> ([Prefix; MAX_PREFIXES], usize) {
    match ip.to_canonical() {
        IpAddr::V4(v4) => ([Prefix::V4(u32::from(v4)); MAX_PREFIXES], 1),
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let prefixes = V6_QUOTA_MULTIPLIERS.map(|(len, _)| Prefix::V6 {
                bits: bits & prefix_mask(len),
                len,
            });
            (prefixes, MAX_PREFIXES)
        }
    }
}

/// The netmask of an IPv6 prefix of `len` bits.
fn prefix_mask(len: u8) -> u128 {
    let host_bits = 128u32.saturating_sub(u32::from(len));
    u128::MAX.checked_shl(host_bits).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arithmetic_side_effects)]

    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse::<Ipv4Addr>().unwrap())
    }

    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse::<Ipv6Addr>().unwrap())
    }

    fn limiter() -> RateLimiter {
        RateLimiter::new(RATE_LIMIT_MAX_KEYS, RATE_LIMIT_IDLE_EXPIRY)
    }

    /// Sends `n` requests at `now` and returns how many were allowed.
    fn burst(l: &RateLimiter, class: RateClass, ip: IpAddr, now: Instant, n: usize) -> usize {
        (0..n)
            .filter(|_| l.check(class, ip, now) == Verdict::Allow)
            .count()
    }

    fn denied(v: Verdict) -> bool {
        matches!(v, Verdict::Deny(_))
    }

    #[test]
    fn quotas_match_the_design() {
        assert_eq!(PAGE_QUOTA.burst, 30);
        assert_eq!(PAGE_QUOTA.interval, Duration::from_nanos(333_333_333));
        assert_eq!(API_QUOTA.burst, 20);
        assert_eq!(API_QUOTA.interval, Duration::from_millis(500));
        let wide = API_QUOTA.scaled(16);
        assert_eq!(wide.burst, 320);
        assert_eq!(wide.interval, Duration::from_micros(31_250));
    }

    #[test]
    fn ipv4_burst_then_refill() {
        let l = limiter();
        let t0 = Instant::now();
        let ip = v4("198.51.100.1");
        assert_eq!(burst(&l, RateClass::Page, ip, t0, 30), 30);
        assert_eq!(
            l.check(RateClass::Page, ip, t0),
            Verdict::Deny(PAGE_QUOTA.interval)
        );
        // One interval later exactly one more request fits.
        let t1 = t0 + PAGE_QUOTA.interval;
        assert_eq!(burst(&l, RateClass::Page, ip, t1, 3), 1);
        // Another client is unaffected.
        assert_eq!(burst(&l, RateClass::Page, v4("198.51.100.2"), t0, 30), 30);
    }

    #[test]
    fn classes_are_independent() {
        let l = limiter();
        let t0 = Instant::now();
        let ip = v4("198.51.100.3");
        assert_eq!(burst(&l, RateClass::Api, ip, t0, 25), 20);
        assert_eq!(burst(&l, RateClass::Page, ip, t0, 35), 30);
    }

    #[test]
    fn ipv6_64_is_limited_with_the_class_quota() {
        let l = limiter();
        let t0 = Instant::now();
        // Any address inside one /64 shares its bucket.
        for i in 0..30u16 {
            let ip = v6(&format!("2001:db8:3:1::{:x}", i + 1));
            assert_eq!(l.check(RateClass::Page, ip, t0), Verdict::Allow, "{i}");
        }
        // The /56 and /48 still have room, so the /64 alone refuses this.
        assert!(denied(l.check(
            RateClass::Page,
            v6("2001:db8:3:1::ffff"),
            t0
        )));
        // A neighbouring /64 is allowed.
        assert_eq!(
            l.check(RateClass::Page, v6("2001:db8:3:2::1"), t0),
            Verdict::Allow
        );
    }

    #[test]
    fn ipv6_56_is_limited_across_its_64s() {
        let l = limiter();
        let t0 = Instant::now();
        // Four /64s inside 2001:db8:6::/56 use up its burst of 4 × 30.
        for net in 0..4u16 {
            let ip = v6(&format!("2001:db8:6:{net:x}::1"));
            assert_eq!(burst(&l, RateClass::Page, ip, t0, 30), 30);
        }
        // A fresh /64 in that /56 is refused although its own bucket is
        // empty and the /48 (16 × 30) still has room.
        assert!(denied(l.check(RateClass::Page, v6("2001:db8:6:ff::1"), t0)));
        // A fresh /56 in the same /48 is allowed.
        assert_eq!(
            l.check(RateClass::Page, v6("2001:db8:6:100::1"), t0),
            Verdict::Allow
        );
    }

    /// Uses up the /48 `2001:db8:<net>::/48` with 30 requests from each of
    /// sixteen /64s in sixteen different /56s.
    fn exhaust_48(l: &RateLimiter, net: u16, now: Instant) {
        for sub in 0..16u16 {
            let ip = v6(&format!("2001:db8:{net:x}:{:x}::1", sub << 8));
            assert_eq!(burst(l, RateClass::Page, ip, now, 30), 30, "{sub}");
        }
    }

    #[test]
    fn ipv6_48_is_limited_across_its_56s() {
        let l = limiter();
        let t0 = Instant::now();
        exhaust_48(&l, 1, t0);
        // A fresh /64 in a fresh /56 of the same /48 is refused.
        assert!(denied(l.check(
            RateClass::Page,
            v6("2001:db8:1:ff00::1"),
            t0
        )));
        // A different /48 is not affected.
        assert_eq!(
            l.check(RateClass::Page, v6("2001:db8:2::1"), t0),
            Verdict::Allow
        );
    }

    #[test]
    fn a_refused_request_charges_no_bucket() {
        let l = limiter();
        let t0 = Instant::now();
        exhaust_48(&l, 4, t0);
        let fresh = v6("2001:db8:4:ff00::1");
        for _ in 0..50 {
            assert!(denied(l.check(RateClass::Page, fresh, t0)));
        }
        // Once the /48 has refilled, the fresh /64 still has its whole
        // burst: the refusals above charged nothing.
        let later = t0 + PAGE_QUOTA.interval * 30;
        assert_eq!(burst(&l, RateClass::Page, fresh, later, 31), 30);
    }

    #[test]
    fn ipv4_mapped_ipv6_is_limited_as_ipv4() {
        let l = limiter();
        let t0 = Instant::now();
        assert_eq!(
            burst(&l, RateClass::Page, v6("::ffff:198.51.100.9"), t0, 30),
            30
        );
        assert!(denied(l.check(RateClass::Page, v4("198.51.100.9"), t0)));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn full_map_sends_new_clients_to_an_overflow_bucket() {
        let l = RateLimiter::new(2, RATE_LIMIT_IDLE_EXPIRY);
        let t0 = Instant::now();
        let a = v4("198.51.100.10");
        let b = v4("198.51.100.11");
        assert_eq!(burst(&l, RateClass::Page, a, t0, 30), 30);
        assert_eq!(l.check(RateClass::Page, b, t0), Verdict::Allow);
        assert_eq!(l.len(), 2);

        // Two new clients in one /16 share an overflow bucket's burst of 30.
        let c = v4("198.51.100.12");
        let d = v4("198.51.100.13");
        assert_eq!(burst(&l, RateClass::Page, c, t0, 20), 20);
        assert_eq!(burst(&l, RateClass::Page, d, t0, 20), 10);
        assert!(denied(l.check(RateClass::Page, v4("198.51.100.14"), t0)));
        // The tracked clients keep their own buckets.
        assert!(denied(l.check(RateClass::Page, a, t0)));
        assert_eq!(l.len(), 2);

        // Two seconds later b has refilled and is forgotten, so c gets a
        // bucket of its own. a has not refilled and keeps its bucket.
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(burst(&l, RateClass::Page, c, t1, 30), 30);
        assert_eq!(l.len(), 2);
        assert_eq!(burst(&l, RateClass::Page, a, t1, 30), 6);

        // Idle entries are swept, and new clients get buckets again.
        let t2 = t1 + RATE_LIMIT_IDLE_EXPIRY + Duration::from_secs(1);
        assert_eq!(burst(&l, RateClass::Page, d, t2, 30), 30);
        assert_eq!(l.len(), 1);
        assert_eq!(l.check(RateClass::Page, b, t2), Verdict::Allow);
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn one_network_drains_only_its_own_overflow_bucket() {
        let l = RateLimiter::new(8, RATE_LIMIT_IDLE_EXPIRY);
        let t0 = Instant::now();
        let attacker = |net: u16| v6(&format!("2001:db8:{net:x}::1"));
        // Fresh /48s of one /32 fill the map and then drain their overflow
        // bucket.
        for net in 0..100u16 {
            let _ = l.check(RateClass::Page, attacker(net), t0);
        }
        assert_eq!(l.len(), 8);
        assert!(denied(l.check(RateClass::Page, attacker(500), t0)));
        // A client of another network in another shard is still served.
        let shard = l.shard_of(attacker(0));
        let other = (0..=255u8)
            .map(|n| IpAddr::V4(Ipv4Addr::new(10, n, 0, 1)))
            .find(|ip| l.shard_of(*ip) != shard)
            .unwrap();
        assert_eq!(burst(&l, RateClass::Page, other, t0, 30), 30);
    }

    #[test]
    fn a_full_map_forgets_refilled_buckets() {
        let l = RateLimiter::new(4, RATE_LIMIT_IDLE_EXPIRY);
        let t0 = Instant::now();
        // One network fills the map and drains the overflow from fresh /48s.
        for net in 0..40u16 {
            let _ = l.check(RateClass::Page, v6(&format!("2001:db8:{net:x}::1")), t0);
        }
        // Two seconds later every one of its buckets has refilled, which is
        // the same as forgetting it: a new client gets its own bucket.
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(burst(&l, RateClass::Page, v4("198.51.100.30"), t1, 30), 30);
    }

    #[test]
    fn a_full_sweep_keeps_buckets_still_in_debt() {
        // Directly pins the sweep predicate: buckets whose GCRA debt is
        // not yet repaid survive a full sweep; only refilled ones go. No
        // debt is forgiven early (a dropped-then-recreated bucket would
        // hand out a fresh burst).
        let t0 = Instant::now();
        let mut st = State {
            buckets: HashMap::new(),
            overflow: HashMap::new(),
            last_sweep: None,
        };
        let indebted = (RateClass::Page, Prefix::V4(0xC633_6401));
        st.buckets.insert(
            indebted,
            Bucket {
                tat: t0 + Duration::from_secs(8),
                seen: t0,
            },
        );
        let repaid = (RateClass::Page, Prefix::V4(0xC633_6402));
        st.buckets.insert(repaid, Bucket { tat: t0, seen: t0 });
        st.sweep_if_due(t0 + Duration::from_secs(2), RATE_LIMIT_IDLE_EXPIRY, true);
        assert!(st.buckets.contains_key(&indebted));
        assert!(!st.buckets.contains_key(&repaid));
    }

    #[test]
    fn ipv6_request_partly_in_overflow_charges_overflow_once() {
        // Room for one entry: the /64 gets it, the wider prefixes overflow.
        let l = RateLimiter::new(1, RATE_LIMIT_IDLE_EXPIRY);
        let t0 = Instant::now();
        assert_eq!(burst(&l, RateClass::Page, v6("2001:db8:5::1"), t0, 30), 30);
        assert_eq!(l.len(), 1);
        assert!(denied(l.check(RateClass::Page, v6("2001:db8:5::2"), t0)));
        // Thirty requests charged the overflow bucket thirty times (not
        // sixty), which is exactly its burst: nothing is left for others.
        // (Another /48 of the same /32 uses the same overflow bucket.)
        assert!(denied(l.check(RateClass::Page, v6("2001:db8:6::1"), t0)));
    }

    #[test]
    fn idle_entries_are_swept_periodically() {
        let l = limiter();
        let t0 = Instant::now();
        for i in 0..100u8 {
            let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, i));
            assert_eq!(l.check(RateClass::Api, ip, t0), Verdict::Allow);
        }
        assert_eq!(l.len(), 100);
        let later = t0 + RATE_LIMIT_IDLE_EXPIRY + SWEEP_INTERVAL;
        assert_eq!(
            l.check(RateClass::Api, v4("203.0.113.1"), later),
            Verdict::Allow
        );
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn prefixes_and_masks() {
        assert_eq!(prefix_mask(128), u128::MAX);
        assert_eq!(prefix_mask(0), 0);
        assert_eq!(prefix_mask(64), u128::MAX << 64);
        assert_eq!(prefix_mask(48), u128::MAX << 80);
        let net = |s: &str| u128::from(s.parse::<Ipv6Addr>().unwrap());
        let (p, n) = prefixes_of(v6("2001:db8:aaaa:bbcc:1:2:3:4"));
        assert_eq!(n, 4);
        assert_eq!(
            p,
            [
                Prefix::V6 {
                    bits: net("2001:db8:aaaa:bbcc::"),
                    len: 64
                },
                Prefix::V6 {
                    bits: net("2001:db8:aaaa:bb00::"),
                    len: 56
                },
                Prefix::V6 {
                    bits: net("2001:db8:aaaa::"),
                    len: 48
                },
                Prefix::V6 {
                    bits: net("2001:db8::"),
                    len: 32
                },
            ]
        );
        assert_eq!(p[0].quota(RateClass::Page), PAGE_QUOTA);
        assert_eq!(p[1].quota(RateClass::Page).burst, 120);
        assert_eq!(p[2].quota(RateClass::Page).burst, 480);
        assert_eq!(p[3].quota(RateClass::Page).burst, 1920);
        assert_eq!(Prefix::V4(1).quota(RateClass::Api), API_QUOTA);
    }
}
