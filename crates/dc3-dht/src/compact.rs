//! Compact node and peer encodings (BEP 5, BEP 32) and the address chokepoint.
//!
//! A compact node list whose length is not a multiple of the entry size is
//! discarded wholesale. [`is_dialable`] is the single filter for every
//! address the node sends to, stores or hands out, so a malicious node
//! cannot steer the crawler at private or reserved networks.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::node_id::{ID_LEN, NodeId};

/// Compact IPv4 node: 20-byte ID, 4-byte address, 2-byte port.
pub const COMPACT_NODE_V4_LEN: usize = 26;
/// Compact IPv6 node: 20-byte ID, 16-byte address, 2-byte port.
pub const COMPACT_NODE_V6_LEN: usize = 38;
/// Compact IPv4 peer: 4-byte address, 2-byte port.
pub const COMPACT_PEER_V4_LEN: usize = 6;
/// Compact IPv6 peer: 16-byte address, 2-byte port.
pub const COMPACT_PEER_V6_LEN: usize = 18;

/// IPv4 prefix length within which two routing-table entries of one bucket conflict.
pub const SUBNET_PREFIX_V4: u32 = 24;
/// IPv6 prefix length within which two routing-table entries of one bucket conflict.
pub const SUBNET_PREFIX_V6: u32 = 64;
/// IPv4 prefix length of one external-IP voter.
pub const VOTER_PREFIX_V4: u32 = 24;
/// IPv6 prefix length of one external-IP voter.
pub const VOTER_PREFIX_V6: u32 = 48;
/// IPv6 prefix length that per-host rate limits and quotas treat as one
/// host: hosts routinely hold a whole /64.
pub const HOST_PREFIX_V6: u32 = 64;

/// Bits in an IPv4 address.
const V4_BITS: u32 = 32;
/// Bits in an IPv6 address.
const V6_BITS: u32 = 128;
/// Offset of the IPv4 address inside a 6to4 address (after the 16-bit prefix).
const SIX_TO_FOUR_V4_SHIFT: u32 = 80;
/// Offset of the Teredo server IPv4 address (after the 32-bit prefix).
const TEREDO_SERVER_SHIFT: u32 = 64;
/// `::1`, the largest IPv6 address that is not IPv4-compatible.
const V6_LOOPBACK_BITS: u128 = 1;

/// Address family of a socket, routing table or endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    /// Both families, IPv4 first.
    pub const ALL: [Family; 2] = [Family::V4, Family::V6];

    /// The family of an endpoint. Call [`canonical_addr`] first if an
    /// IPv4-mapped address should count as IPv4.
    pub fn of(addr: &SocketAddr) -> Family {
        Family::of_ip(&addr.ip())
    }

    /// The family of an address.
    pub fn of_ip(ip: &IpAddr) -> Family {
        match ip {
            IpAddr::V4(_) => Family::V4,
            IpAddr::V6(_) => Family::V6,
        }
    }

    /// Size of one compact node entry of this family.
    pub fn node_len(self) -> usize {
        match self {
            Family::V4 => COMPACT_NODE_V4_LEN,
            Family::V6 => COMPACT_NODE_V6_LEN,
        }
    }

    /// The metric label: `"v4"` or `"v6"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Family::V4 => "v4",
            Family::V6 => "v6",
        }
    }
}

/// A node as carried in `nodes` / `nodes6`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompactNode {
    pub id: NodeId,
    pub addr: SocketAddr,
}

/// Decodes a 6-byte (IPv4) or 18-byte (IPv6) compact endpoint.
pub fn decode_peer(bytes: &[u8]) -> Option<SocketAddr> {
    match bytes.len() {
        COMPACT_PEER_V4_LEN => {
            let ip: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
            let port: [u8; 2] = bytes.get(4..)?.try_into().ok()?;
            let addr = SocketAddrV4::new(Ipv4Addr::from(ip), u16::from_be_bytes(port));
            Some(SocketAddr::V4(addr))
        }
        COMPACT_PEER_V6_LEN => {
            let ip: [u8; 16] = bytes.get(..16)?.try_into().ok()?;
            let port: [u8; 2] = bytes.get(16..)?.try_into().ok()?;
            let addr = SocketAddrV6::new(Ipv6Addr::from(ip), u16::from_be_bytes(port), 0, 0);
            Some(SocketAddr::V6(addr))
        }
        _ => None,
    }
}

/// Decodes a top-level BEP 42 `ip` field: the sender's address as raw
/// bytes (4 IPv4 or 16 IPv6, port unknown, reported with port 0) or as a
/// compact endpoint (6 or 18 bytes, with port). Anything else is ignored.
pub fn decode_bep42_ip(bytes: &[u8]) -> Option<SocketAddr> {
    match bytes.len() {
        4 => {
            let ip: [u8; 4] = bytes.try_into().ok()?;
            Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), 0)))
        }
        16 => {
            let ip: [u8; 16] = bytes.try_into().ok()?;
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(ip),
                0,
                0,
                0,
            )))
        }
        COMPACT_PEER_V4_LEN | COMPACT_PEER_V6_LEN => decode_peer(bytes),
        _ => None,
    }
}

/// Writes the compact form of `addr` into `scratch` (6 bytes for IPv4,
/// 18 for IPv6) and returns the number of bytes written, so hot encode
/// paths can reuse one stack buffer instead of heap-allocating per peer.
pub fn encode_peer_scratch(addr: &SocketAddr, scratch: &mut [u8; COMPACT_PEER_V6_LEN]) -> usize {
    match addr {
        SocketAddr::V4(a) => scratch[..4].copy_from_slice(&a.ip().octets()),
        SocketAddr::V6(a) => scratch[..16].copy_from_slice(&a.ip().octets()),
    }
    let n = match addr {
        SocketAddr::V4(_) => COMPACT_PEER_V4_LEN,
        SocketAddr::V6(_) => COMPACT_PEER_V6_LEN,
    };
    scratch[n - 2..n].copy_from_slice(&addr.port().to_be_bytes());
    n
}

/// Appends the compact form of `addr` (6 or 18 bytes) to `out`.
pub fn encode_peer_into(addr: &SocketAddr, out: &mut Vec<u8>) {
    let mut scratch = [0u8; COMPACT_PEER_V6_LEN];
    let n = encode_peer_scratch(addr, &mut scratch);
    out.extend_from_slice(&scratch[..n]);
}

/// The compact form of `addr` (6 or 18 bytes).
pub fn encode_peer(addr: &SocketAddr) -> Vec<u8> {
    let mut scratch = [0u8; COMPACT_PEER_V6_LEN];
    let n = encode_peer_scratch(addr, &mut scratch);
    scratch[..n].to_vec()
}

/// Decodes a compact node list of the given family. Returns `None` when the
/// length is not a multiple of the entry size.
pub fn decode_nodes(bytes: &[u8], family: Family) -> Option<Vec<CompactNode>> {
    let size = family.node_len();
    if !bytes.len().is_multiple_of(size) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len().checked_div(size).unwrap_or(0));
    for chunk in bytes.chunks_exact(size) {
        let id = NodeId::from_slice(chunk.get(..ID_LEN)?)?;
        let addr = decode_peer(chunk.get(ID_LEN..)?)?;
        out.push(CompactNode { id, addr });
    }
    Some(out)
}

/// Encodes the nodes of `family` as one compact byte string; others are skipped.
pub fn encode_nodes(nodes: &[CompactNode], family: Family) -> Vec<u8> {
    let mut out = Vec::with_capacity(nodes.len().saturating_mul(family.node_len()));
    for node in nodes.iter().filter(|n| Family::of(&n.addr) == family) {
        out.extend_from_slice(&node.id.0);
        encode_peer_into(&node.addr, &mut out);
    }
    out
}

/// IPv4 ranges that are never dialable (network, prefix length).
const V4_NOT_DIALABLE: [(u32, u32); 15] = [
    (0x0000_0000, 8),  // 0.0.0.0/8 "this network", including 0.0.0.0
    (0x0a00_0000, 8),  // 10.0.0.0/8 private (RFC 1918)
    (0x6440_0000, 10), // 100.64.0.0/10 carrier-grade NAT
    (0x7f00_0000, 8),  // 127.0.0.0/8 loopback
    (0xa9fe_0000, 16), // 169.254.0.0/16 link-local
    (0xac10_0000, 12), // 172.16.0.0/12 private (RFC 1918)
    (0xc000_0000, 24), // 192.0.0.0/24 IETF protocol assignments
    (0xc000_0200, 24), // 192.0.2.0/24 documentation
    (0xc058_6300, 24), // 192.88.99.0/24 deprecated 6to4 relay anycast
    (0xc0a8_0000, 16), // 192.168.0.0/16 private (RFC 1918)
    (0xc612_0000, 15), // 198.18.0.0/15 benchmarking
    (0xc633_6400, 24), // 198.51.100.0/24 documentation
    (0xcb00_7100, 24), // 203.0.113.0/24 documentation
    (0xe000_0000, 4),  // 224.0.0.0/4 multicast
    (0xf000_0000, 4),  // 240.0.0.0/4 reserved, including 255.255.255.255
];

/// Global unicast IPv6 space (2000::/3). Everything else is special-purpose
/// or reserved (::/128, ::1, 100::/64, fc00::/7, fe80::/10, ff00::/8, ...),
/// except the NAT64 prefix, which is checked first.
const V6_GLOBAL_UNICAST: (u128, u32) = (0x2000 << 112, 3);
/// Teredo (RFC 4380), 2001::/32: the server IPv4 address follows the
/// prefix and the client IPv4 address, inverted, is the last 32 bits.
const V6_TEREDO: (u128, u32) = (0x2001_0000 << 96, 32);
/// 6to4 (RFC 3056), 2002::/16: the IPv4 address follows the prefix.
const V6_6TO4: (u128, u32) = (0x2002 << 112, 16);
/// NAT64 well-known prefix (RFC 6052), 64:ff9b::/96: the IPv4 address is the last 32 bits.
const V6_NAT64: (u128, u32) = (0x0064_ff9b << 96, 96);
/// Ranges inside 2000::/3 that are never dialable. Teredo and 6to4 are
/// checked before this list.
const V6_NOT_DIALABLE: [(u128, u32); 3] = [
    (0x2001 << 112, 23),     // 2001::/23 IETF protocol assignments
    (0x2001_0db8 << 96, 32), // 2001:db8::/32 documentation
    (0x3fff << 112, 20),     // 3fff::/20 documentation (RFC 9637)
];

fn v4_mask(len: u32) -> u32 {
    u32::MAX
        .checked_shl(V4_BITS.saturating_sub(len))
        .unwrap_or(0)
}

fn v6_mask(len: u32) -> u128 {
    u128::MAX
        .checked_shl(V6_BITS.saturating_sub(len))
        .unwrap_or(0)
}

fn v4_in(ip: u32, (net, len): (u32, u32)) -> bool {
    ip & v4_mask(len) == net & v4_mask(len)
}

fn v6_in(ip: u128, (net, len): (u128, u32)) -> bool {
    ip & v6_mask(len) == net & v6_mask(len)
}

/// The IPv4 address in the low 32 bits of `bits`.
fn low_v4(bits: u128) -> Ipv4Addr {
    // The mask keeps 32 bits, so the conversion cannot fail.
    Ipv4Addr::from(u32::try_from(bits & u128::from(u32::MAX)).unwrap_or(0))
}

fn v4_is_global(ip: Ipv4Addr) -> bool {
    let bits = u32::from(ip);
    !V4_NOT_DIALABLE.iter().any(|range| v4_in(bits, *range))
}

fn v6_is_global(ip: Ipv6Addr) -> bool {
    let bits = u128::from(ip);
    if v6_in(bits, V6_NAT64) {
        return v4_is_global(low_v4(bits));
    }
    if !v6_in(bits, V6_GLOBAL_UNICAST) {
        return false;
    }
    if v6_in(bits, V6_TEREDO) {
        let server = low_v4(bits >> TEREDO_SERVER_SHIFT);
        let client = low_v4(!bits);
        return v4_is_global(server) && v4_is_global(client);
    }
    if v6_in(bits, V6_6TO4) {
        return v4_is_global(low_v4(bits >> SIX_TO_FOUR_V4_SHIFT));
    }
    !V6_NOT_DIALABLE.iter().any(|range| v6_in(bits, *range))
}

/// The IPv4 address that an IPv4-mapped (`::ffff:a.b.c.d`) or
/// IPv4-compatible (`::a.b.c.d`) IPv6 address stands for. Other addresses,
/// including `::` and `::1`, are returned unchanged.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v6) = ip else { return ip };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return IpAddr::V4(v4);
    }
    let bits = u128::from(v6);
    if bits >> V4_BITS == 0 && bits > V6_LOOPBACK_BITS {
        return IpAddr::V4(low_v4(bits));
    }
    ip
}

/// `addr` with its IP made canonical (see [`canonical_ip`]).
pub fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    let ip = canonical_ip(addr.ip());
    if ip == addr.ip() {
        addr
    } else {
        SocketAddr::new(ip, addr.port())
    }
}

/// True for a publicly routable address (see [`is_dialable`] for the rules).
pub fn is_global_ip(ip: IpAddr) -> bool {
    match canonical_ip(ip) {
        IpAddr::V4(v4) => v4_is_global(v4),
        IpAddr::V6(v6) => v6_is_global(v6),
    }
}

/// The address chokepoint: whether the node may send to, store or hand out
/// `addr`.
///
/// IPv4-mapped and IPv4-compatible IPv6 addresses are checked as IPv4. Port
/// 0 and unspecified addresses are always rejected. Unless `allow_private`
/// is set (tests only), these are rejected too:
/// * IPv4: 0/8, 10/8, 100.64/10, 127/8, 169.254/16, 172.16/12, 192.0.0/24,
///   192.0.2/24, 192.88.99/24, 192.168/16, 198.18/15, 198.51.100/24,
///   203.0.113/24, 224/4 (multicast) and 240/4 (reserved and broadcast);
/// * IPv6: everything outside 2000::/3 (::, ::1, 100::/64, fc00::/7,
///   fe80::/10, ff00::/8 and reserved space), 2001::/23, 2001:db8::/32 and
///   3fff::/20;
/// * Teredo (2001::/32), 6to4 (2002::/16) and NAT64 (64:ff9b::/96)
///   addresses whose embedded IPv4 addresses fail the IPv4 rules.
pub fn is_dialable(addr: SocketAddr, allow_private: bool) -> bool {
    addr.port() != 0 && is_dialable_ip(addr.ip(), allow_private)
}

/// [`is_dialable`] without the port check.
pub(crate) fn is_dialable_ip(ip: IpAddr, allow_private: bool) -> bool {
    let ip = canonical_ip(ip);
    if allow_private {
        !ip.is_unspecified()
    } else {
        is_global_ip(ip)
    }
}

/// `ip` with every bit after the family's prefix length cleared.
fn network(ip: IpAddr, v4_len: u32, v6_len: u32) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(Ipv4Addr::from(u32::from(v4) & v4_mask(v4_len))),
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & v6_mask(v6_len))),
    }
}

/// The identity that a per-address rule counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AddrKey {
    /// An address or a network (production).
    Ip(IpAddr),
    /// A whole endpoint (`limits_by_endpoint`, tests only).
    Endpoint(SocketAddr),
}

/// How the per-address rules treat addresses (design §7, "Test support").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AddrPolicy {
    /// Accept non-global addresses (`allow_private_addrs`).
    pub(crate) allow_private: bool,
    /// Key the per-IP rules on IP:port (`limits_by_endpoint`).
    pub(crate) by_endpoint: bool,
}

impl AddrPolicy {
    /// [`is_dialable`] under this policy.
    pub(crate) fn dialable(self, addr: &SocketAddr) -> bool {
        is_dialable(*addr, self.allow_private)
    }

    /// [`is_dialable`] for an address without a port.
    pub(crate) fn dialable_ip(self, ip: IpAddr) -> bool {
        is_dialable_ip(ip, self.allow_private)
    }

    /// The key for "one entry per IP" and per-IP rate limits.
    pub(crate) fn key(self, addr: &SocketAddr) -> AddrKey {
        let addr = canonical_addr(*addr);
        if self.by_endpoint {
            AddrKey::Endpoint(addr)
        } else {
            AddrKey::Ip(addr.ip())
        }
    }

    /// The key of per-host rate limits and quotas: the IP (IPv4) or its
    /// /64 (IPv6).
    pub(crate) fn host_key(self, addr: &SocketAddr) -> AddrKey {
        self.prefix_key(addr, V4_BITS, HOST_PREFIX_V6)
    }

    /// The key of inbound rate limits: the IP (IPv4) or its /48 (IPv6),
    /// the same actor the external-IP vote counts ([`Self::voter_key`]).
    /// A /48 holder rotating source addresses shares one bucket.
    pub(crate) fn inbound_key(self, addr: &SocketAddr) -> AddrKey {
        self.prefix_key(addr, V4_BITS, VOTER_PREFIX_V6)
    }

    /// The key of an external-IP voter, and of one source network in
    /// per-network quotas: its /24 (IPv4) or /48 (IPv6).
    pub(crate) fn voter_key(self, addr: &SocketAddr) -> AddrKey {
        self.prefix_key(addr, VOTER_PREFIX_V4, VOTER_PREFIX_V6)
    }

    /// The key of the routing-table subnet rule: /24 (IPv4) or /64 (IPv6).
    pub(crate) fn subnet_key(self, addr: &SocketAddr) -> AddrKey {
        self.prefix_key(addr, SUBNET_PREFIX_V4, SUBNET_PREFIX_V6)
    }

    fn prefix_key(self, addr: &SocketAddr, v4_len: u32, v6_len: u32) -> AddrKey {
        let addr = canonical_addr(*addr);
        if self.by_endpoint {
            AddrKey::Endpoint(addr)
        } else {
            AddrKey::Ip(network(addr.ip(), v4_len, v6_len))
        }
    }

    /// Whether two entries may not share a routing-table bucket: same /24
    /// (IPv4) or /64 (IPv6).
    pub(crate) fn same_subnet(self, a: &SocketAddr, b: &SocketAddr) -> bool {
        self.subnet_key(a) == self.subnet_key(b)
    }
}

/// Our own addresses: the bound IPs that are not unspecified and the voted
/// external IPs, with and without our ports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OwnAddrs {
    pub(crate) ips: Vec<IpAddr>,
    pub(crate) endpoints: Vec<SocketAddr>,
}

impl OwnAddrs {
    /// Adds `addr` unless its IP is unspecified.
    pub(crate) fn add(&mut self, addr: SocketAddr) {
        let addr = canonical_addr(addr);
        if addr.ip().is_unspecified() {
            return;
        }
        if !self.ips.contains(&addr.ip()) {
            self.ips.push(addr.ip());
        }
        if !self.endpoints.contains(&addr) {
            self.endpoints.push(addr);
        }
    }

    /// Whether `addr` is ours: the same IP, or the same endpoint with
    /// `limits_by_endpoint`.
    pub(crate) fn contains(&self, addr: &SocketAddr, policy: AddrPolicy) -> bool {
        let addr = canonical_addr(*addr);
        if policy.by_endpoint {
            self.endpoints.contains(&addr)
        } else {
            self.ips.contains(&addr.ip())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    const PRODUCTION: AddrPolicy = AddrPolicy {
        allow_private: false,
        by_endpoint: false,
    };
    const TEST: AddrPolicy = AddrPolicy {
        allow_private: true,
        by_endpoint: true,
    };

    #[test]
    fn bep42_ip_forms() {
        assert_eq!(decode_bep42_ip(&[1, 2, 3, 4]), Some(sa("1.2.3.4:0")));
        assert_eq!(
            decode_bep42_ip(&[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            Some(sa("[2001:db8::1]:0"))
        );
        // Compact endpoints with ports still decode with their port.
        assert_eq!(decode_bep42_ip(&[1, 2, 3, 4, 0, 5]), Some(sa("1.2.3.4:5")));
        // Anything else is ignored.
        for bad in [&[][..], &[0; 3], &[0; 5], &[0; 7], &[0; 17], &[0; 19]] {
            assert_eq!(decode_bep42_ip(bad), None, "len={}", bad.len());
        }
    }

    #[test]
    fn peer_round_trip() {
        for s in ["1.2.3.4:6881", "[2001:4860::1]:51413"] {
            let a = sa(s);
            assert_eq!(decode_peer(&encode_peer(&a)), Some(a));
        }
        assert_eq!(encode_peer(&sa("1.2.3.4:258")), vec![1, 2, 3, 4, 1, 2]);
        assert_eq!(decode_peer(&[0; 5]), None);
        assert_eq!(decode_peer(&[0; 7]), None);
        assert_eq!(decode_peer(&[]), None);
    }

    #[test]
    fn scratch_writer_matches_allocating_encoder() {
        for (s, n) in [
            ("1.2.3.4:6881", COMPACT_PEER_V4_LEN),
            ("0.0.0.0:0", COMPACT_PEER_V4_LEN),
            ("[2001:4860::1]:51413", COMPACT_PEER_V6_LEN),
            ("[::]:0", COMPACT_PEER_V6_LEN),
            ("[::ffff:9.9.9.9]:53", COMPACT_PEER_V6_LEN),
        ] {
            let a = sa(s);
            let mut scratch = [0xAAu8; COMPACT_PEER_V6_LEN];
            assert_eq!(encode_peer_scratch(&a, &mut scratch), n, "{s}");
            assert_eq!(&scratch[..n], &encode_peer(&a)[..], "{s}");
            assert!(scratch[n..].iter().all(|&b| b == 0xAA), "{s}");
        }
    }

    #[test]
    fn nodes_round_trip_and_bad_lengths() {
        let v4 = CompactNode {
            id: NodeId([7; 20]),
            addr: sa("5.6.7.8:1000"),
        };
        let v6 = CompactNode {
            id: NodeId([9; 20]),
            addr: sa("[2a00::1]:2000"),
        };
        let enc4 = encode_nodes(&[v4, v6], Family::V4);
        assert_eq!(enc4.len(), 26);
        assert_eq!(decode_nodes(&enc4, Family::V4), Some(vec![v4]));
        let enc6 = encode_nodes(&[v4, v6], Family::V6);
        assert_eq!(enc6.len(), 38);
        assert_eq!(decode_nodes(&enc6, Family::V6), Some(vec![v6]));
        let mut bad = enc4.clone();
        bad.push(0);
        assert_eq!(decode_nodes(&bad, Family::V4), None);
        assert_eq!(decode_nodes(&enc6, Family::V4), None);
        assert_eq!(decode_nodes(&[], Family::V4), Some(vec![]));
    }

    #[test]
    fn families() {
        assert_eq!(Family::of(&sa("1.2.3.4:1")), Family::V4);
        assert_eq!(Family::of(&sa("[::ffff:1.2.3.4]:1")), Family::V6);
        assert_eq!(
            Family::of(&canonical_addr(sa("[::ffff:1.2.3.4]:1"))),
            Family::V4
        );
        assert_eq!(Family::ALL.map(Family::as_str), ["v4", "v6"]);
    }

    #[test]
    fn canonical_forms() {
        assert_eq!(canonical_ip(ip("::ffff:8.8.8.8")), ip("8.8.8.8"));
        assert_eq!(canonical_ip(ip("::8.8.8.8")), ip("8.8.8.8"));
        assert_eq!(canonical_ip(ip("::2")), ip("0.0.0.2"));
        // The unspecified and loopback addresses are not IPv4-compatible.
        assert_eq!(canonical_ip(ip("::")), ip("::"));
        assert_eq!(canonical_ip(ip("::1")), ip("::1"));
        assert_eq!(canonical_ip(ip("2a00::1")), ip("2a00::1"));
        assert_eq!(canonical_ip(ip("1.2.3.4")), ip("1.2.3.4"));
        assert_eq!(canonical_addr(sa("[::ffff:1.2.3.4]:9")), sa("1.2.3.4:9"));
        let scoped = SocketAddr::V6(SocketAddrV6::new(ip6("fe80::1"), 5, 0, 3));
        assert_eq!(canonical_addr(scoped), scoped);
    }

    fn ip6(s: &str) -> Ipv6Addr {
        s.parse().unwrap()
    }

    /// A Teredo address for the given server and client IPv4 addresses.
    fn teredo(server: [u8; 4], client: [u8; 4]) -> SocketAddr {
        let server = u128::from(u32::from_be_bytes(server));
        let client = u128::from(!u32::from_be_bytes(client));
        let bits = (0x2001_0000_u128 << 96)
            | (server << 64)
            | (0x8000_u128 << 48)
            | (0xf227_u128 << 32)
            | client;
        SocketAddr::new(IpAddr::V6(Ipv6Addr::from(bits)), 1)
    }

    #[test]
    fn dialable_addresses() {
        let rejected = [
            "0.0.0.0:1",
            "0.1.2.3:1",
            "10.0.0.1:1",
            "100.64.0.1:1",
            "100.127.255.255:1",
            "127.0.0.1:1",
            "169.254.1.1:1",
            "172.16.0.1:1",
            "172.31.255.255:1",
            "192.0.0.1:1",
            "192.0.0.255:1",
            "192.0.2.1:1",
            "192.88.99.1:1",
            "192.168.0.1:1",
            "198.18.0.1:1",
            "198.19.255.255:1",
            "198.51.100.1:1",
            "203.0.113.1:1",
            "224.0.0.1:1",
            "239.255.255.250:1",
            "240.0.0.1:1",
            "255.255.255.255:1",
            "[::]:1",
            "[::1]:1",
            "[100::1]:1",
            "[100::ffff:ffff:ffff:ffff]:1",
            "[fe80::1]:1",
            "[febf::1]:1",
            "[fc00::1]:1",
            "[fd12:3456::1]:1",
            "[ff02::1]:1",
            "[ff0e::1]:1",
            "[4000::1]:1",
            "[::ffff:10.0.0.1]:1",
            "[::ffff:0.0.0.0]:1",
            "[::10.0.0.1]:1",
            "[2001:db8::1]:1",
            "[2001:db8:ffff::1]:1",
            "[3fff::1]:1",
            "[2001:2::1]:1",
            "[2001:10::1]:1",
            "[2001:1ff::1]:1",
            // 6to4 and NAT64 wrapping non-dialable IPv4 addresses.
            "[2002:c0a8:0101::1]:1",
            "[2002:7f00:1::1]:1",
            "[2002::1]:1",
            "[64:ff9b::10.0.0.1]:1",
            "[64:ff9b::127.0.0.1]:1",
            // NAT64 local-use prefix (RFC 8215) is not global.
            "[64:ff9b:1::8.8.8.8]:1",
            "8.8.8.8:0",
            "[2a00:1450::1]:0",
        ];
        for s in rejected {
            assert!(!is_dialable(sa(s), false), "{s} should be rejected");
        }
        let accepted = [
            "8.8.8.8:53",
            "1.1.1.1:6881",
            "100.63.255.255:1",
            "100.128.0.1:1",
            "172.32.0.1:1",
            "192.0.1.1:1",
            "198.17.255.255:1",
            "198.20.0.1:1",
            "223.255.255.255:1",
            "[2a00:1450::1]:443",
            "[2606:4700::1111]:1",
            "[2001:200::1]:1",
            "[2001:db9::1]:1",
            "[3fff:1000::1]:1",
            "[::ffff:8.8.8.8]:1",
            "[::8.8.8.8]:1",
            "[2002:0808:0808::1]:1",
            "[64:ff9b::8.8.8.8]:1",
        ];
        for s in accepted {
            assert!(is_dialable(sa(s), false), "{s} should be accepted");
        }
    }

    #[test]
    fn teredo_depends_on_embedded_addresses() {
        assert!(is_dialable(teredo([65, 54, 227, 120], [8, 8, 8, 8]), false));
        // A private or reserved server or client makes it non-dialable.
        assert!(!is_dialable(teredo([10, 0, 0, 1], [8, 8, 8, 8]), false));
        assert!(!is_dialable(
            teredo([65, 54, 227, 120], [192, 168, 1, 2]),
            false
        ));
        assert!(!is_dialable(
            teredo([65, 54, 227, 120], [255, 255, 255, 255]),
            false
        ));
        assert!(!is_dialable(teredo([0, 0, 0, 0], [0, 0, 0, 0]), false));
        // The rest of 2001::/23 stays rejected.
        assert!(!is_dialable(sa("[2001:1::1]:1"), false));
    }

    #[test]
    fn allow_private_rejects_only_unspecified_and_port_zero() {
        for s in [
            "127.0.0.1:1",
            "10.0.0.1:1",
            "[::1]:1",
            "[fe80::1]:1",
            "[2001:db8::1]:1",
            "224.0.0.1:1",
        ] {
            assert!(is_dialable(sa(s), true), "{s}");
        }
        for s in [
            "0.0.0.0:1",
            "[::]:1",
            "[::ffff:0.0.0.0]:1",
            "127.0.0.1:0",
            "[::1]:0",
        ] {
            assert!(!is_dialable(sa(s), true), "{s}");
        }
    }

    #[test]
    fn keys_and_subnets() {
        // Production keys on the IP, whatever the address.
        assert_eq!(
            PRODUCTION.key(&sa("8.8.8.8:1")),
            PRODUCTION.key(&sa("8.8.8.8:2"))
        );
        assert_eq!(
            PRODUCTION.key(&sa("127.0.0.1:1")),
            PRODUCTION.key(&sa("127.0.0.1:2"))
        );
        assert_eq!(
            PRODUCTION.key(&sa("[::ffff:8.8.8.8]:1")),
            PRODUCTION.key(&sa("8.8.8.8:2"))
        );
        // Tests key on the endpoint.
        assert_ne!(TEST.key(&sa("127.0.0.1:1")), TEST.key(&sa("127.0.0.1:2")));
        assert_eq!(TEST.key(&sa("127.0.0.1:1")), TEST.key(&sa("127.0.0.1:1")));

        let p = PRODUCTION;
        assert!(p.same_subnet(&sa("8.8.8.1:1"), &sa("8.8.8.200:2")));
        assert!(!p.same_subnet(&sa("8.8.8.1:1"), &sa("8.8.9.1:1")));
        assert!(p.same_subnet(&sa("[2a00:1:2:3::1]:1"), &sa("[2a00:1:2:3:ffff::1]:1")));
        assert!(!p.same_subnet(&sa("[2a00:1:2:3::1]:1"), &sa("[2a00:1:2:4::1]:1")));
        assert!(p.same_subnet(&sa("127.0.0.1:1"), &sa("127.0.0.1:2")));
        assert!(!p.same_subnet(&sa("8.8.8.8:1"), &sa("[2a00::1]:1")));
        assert!(!TEST.same_subnet(&sa("127.0.0.1:1"), &sa("127.0.0.1:2")));
        assert!(TEST.same_subnet(&sa("127.0.0.1:1"), &sa("127.0.0.1:1")));
    }

    #[test]
    fn host_keys_group_ipv6_by_64() {
        let p = PRODUCTION;
        assert_eq!(
            p.host_key(&sa("[2a01:4f8:1:2::1]:1")),
            p.host_key(&sa("[2a01:4f8:1:2:ffff::9]:2"))
        );
        assert_ne!(
            p.host_key(&sa("[2a01:4f8:1:2::1]:1")),
            p.host_key(&sa("[2a01:4f8:1:3::1]:1"))
        );
        assert_ne!(p.host_key(&sa("8.8.8.1:1")), p.host_key(&sa("8.8.8.2:1")));
        assert_eq!(
            p.host_key(&sa("[::ffff:8.8.8.1]:1")),
            p.host_key(&sa("8.8.8.1:2"))
        );
        assert_ne!(
            TEST.host_key(&sa("127.0.0.1:1")),
            TEST.host_key(&sa("127.0.0.1:2"))
        );
    }

    #[test]
    fn voter_keys() {
        let p = PRODUCTION;
        assert_eq!(
            p.voter_key(&sa("8.8.8.1:1")),
            p.voter_key(&sa("8.8.8.254:9"))
        );
        assert_ne!(p.voter_key(&sa("8.8.8.1:1")), p.voter_key(&sa("8.8.9.1:1")));
        assert_eq!(
            p.voter_key(&sa("[2a00:1:2::1]:1")),
            p.voter_key(&sa("[2a00:1:2:ffff::1]:1"))
        );
        assert_ne!(
            p.voter_key(&sa("[2a00:1:2::1]:1")),
            p.voter_key(&sa("[2a00:1:3::1]:1"))
        );
        assert_eq!(
            p.voter_key(&sa("[::ffff:8.8.8.1]:1")),
            p.voter_key(&sa("8.8.8.2:1"))
        );
        assert_ne!(
            TEST.voter_key(&sa("127.0.0.1:1")),
            TEST.voter_key(&sa("127.0.0.1:2"))
        );
    }

    #[test]
    fn own_addresses() {
        let mut own = OwnAddrs::default();
        own.add(sa("0.0.0.0:6881"));
        own.add(sa("[::]:6881"));
        assert_eq!(own, OwnAddrs::default());
        own.add(sa("127.0.0.1:5000"));
        own.add(sa("127.0.0.1:5000"));
        own.add(sa("[::ffff:1.2.3.4]:5000"));
        assert_eq!(own.ips, vec![ip("127.0.0.1"), ip("1.2.3.4")]);
        assert_eq!(
            own.endpoints,
            vec![sa("127.0.0.1:5000"), sa("1.2.3.4:5000")]
        );
        // Production: any port of our IP is ours.
        assert!(own.contains(&sa("127.0.0.1:1"), PRODUCTION));
        assert!(own.contains(&sa("1.2.3.4:9"), PRODUCTION));
        assert!(!own.contains(&sa("1.2.3.5:5000"), PRODUCTION));
        // Tests: only our own endpoints.
        assert!(own.contains(&sa("127.0.0.1:5000"), TEST));
        assert!(!own.contains(&sa("127.0.0.1:5001"), TEST));
    }
}
