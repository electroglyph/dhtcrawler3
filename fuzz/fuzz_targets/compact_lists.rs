#![no_main]
//! Compact node and peer decoding must never panic and must round-trip, and
//! the address chokepoint must be consistent for any address.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use dc4_dht::compact::{
    COMPACT_PEER_V4_LEN, COMPACT_PEER_V6_LEN, canonical_addr, canonical_ip, decode_nodes,
    decode_peer, encode_nodes, encode_peer, is_global_ip,
};
use dc4_dht::{Family, is_dialable};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for family in Family::ALL {
        if let Some(nodes) = decode_nodes(data, family) {
            assert_eq!(nodes.len(), data.len() / family.node_len());
            assert!(nodes.iter().all(|n| Family::of(&n.addr) == family));
            assert_eq!(encode_nodes(&nodes, family), data);
            for n in &nodes {
                check_addr(n.addr);
            }
        } else {
            assert!(!data.len().is_multiple_of(family.node_len()));
        }
    }

    match decode_peer(data) {
        Some(addr) => {
            assert_eq!(encode_peer(&addr), data);
            check_addr(addr);
        }
        None => assert!(data.len() != COMPACT_PEER_V4_LEN && data.len() != COMPACT_PEER_V6_LEN),
    }

    // Every 6- and 18-byte window as an address, so short inputs reach both families.
    for w in data.windows(COMPACT_PEER_V4_LEN).take(64) {
        if let Some(addr) = decode_peer(w) {
            check_addr(addr);
        }
    }
    for w in data.windows(COMPACT_PEER_V6_LEN).take(64) {
        if let Some(addr) = decode_peer(w) {
            check_addr(addr);
            // The same bits as an IPv4-mapped address must be judged as IPv4.
            if let SocketAddr::V6(v6) = addr {
                let bits = u128::from(*v6.ip());
                let v4 = Ipv4Addr::from((bits & u128::from(u32::MAX)) as u32);
                let mapped = SocketAddr::new(v4.to_ipv6_mapped().into(), addr.port());
                let plain = SocketAddr::new(v4.into(), addr.port());
                assert_eq!(is_dialable(mapped, false), is_dialable(plain, false));
            }
        }
    }
});

fn check_addr(addr: SocketAddr) {
    let strict = is_dialable(addr, false);
    let lax = is_dialable(addr, true);
    // Anything dialable in production is dialable in tests.
    assert!(!strict || lax);
    if addr.port() == 0 {
        assert!(!strict && !lax);
    }
    if addr.ip().is_unspecified() || addr.ip() == Ipv6Addr::UNSPECIFIED {
        assert!(!lax);
    }
    let canon = canonical_addr(addr);
    assert_eq!(canonical_addr(canon), canon);
    assert_eq!(canonical_ip(canon.ip()), canon.ip());
    assert_eq!(is_dialable(canon, false), strict);
    assert_eq!(is_dialable(canon, true), lax);
    assert_eq!(is_global_ip(addr.ip()), is_global_ip(canon.ip()));
    if addr.port() != 0 {
        assert_eq!(strict, is_global_ip(addr.ip()));
    }
    if strict {
        let ip = canon.ip();
        assert!(!ip.is_loopback() && !ip.is_multicast() && !ip.is_unspecified());
    }
}
