//! Node IDs: XOR distance (Kademlia) and BEP 42 secure IDs.
//!
//! BEP 42 is implemented as its example code and test vectors define it (the
//! prose disagrees; see `docs/00-horismos.md` §4): the masked IPv4 address is
//! 4 bytes and the masked IPv6 prefix is 8 bytes.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use dc4_core::DhtKey;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::compact;

/// Length of a node ID in bytes.
pub const ID_LEN: usize = 20;
/// Length of a node ID in bits.
pub const ID_BITS: usize = 160;

/// Bits per byte, used for bit-level ID arithmetic.
const BYTE_BITS: usize = 8;
/// Right-shift that selects the most significant bit of a byte.
const TOP_BIT_SHIFT: u32 = 7;
/// BEP 42 mask applied to the 4 bytes of an IPv4 address.
const BEP42_V4_MASK: [u8; 4] = [0x03, 0x0f, 0x3f, 0xff];
/// BEP 42 mask applied to the first 8 bytes of an IPv6 address.
const BEP42_V6_MASK: [u8; 8] = [0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];
/// The random value `r` is the low 3 bits of the ID's last byte.
const BEP42_R_MASK: u8 = 0x07;
/// `r` is placed in the top 3 bits of the first masked byte.
const BEP42_R_SHIFT: u32 = 5;
/// Only the top 5 bits of the third ID byte come from the CRC (21 bits in total).
const BEP42_THIRD_BYTE_MASK: u8 = 0xf8;

/// A 160-bit node ID. It shares its space with DHT keys.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct NodeId(pub [u8; ID_LEN]);

/// The XOR distance between two IDs. Ordering is numeric (smaller is closer).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Distance(pub [u8; ID_LEN]);

impl NodeId {
    /// A uniformly random ID.
    pub fn random() -> Self {
        NodeId(rand::random())
    }

    /// Builds an ID from a slice that must be exactly 20 bytes long.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(NodeId)
    }

    pub fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }

    /// Lowercase 40-character hex.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The XOR distance to `other`.
    pub fn distance(&self, other: &NodeId) -> Distance {
        let mut out = [0u8; ID_LEN];
        for ((o, a), b) in out.iter_mut().zip(self.0.iter()).zip(other.0.iter()) {
            *o = a ^ b;
        }
        Distance(out)
    }

    /// Number of leading bits shared with `other` (0..=160).
    pub fn common_prefix_len(&self, other: &NodeId) -> usize {
        self.distance(other).leading_zeros()
    }

    /// The bit at position `index` (0 = most significant). Out-of-range is `false`.
    pub fn bit(&self, index: usize) -> bool {
        let (byte, shift) = bit_position(index);
        self.0.get(byte).is_some_and(|b| (b >> shift) & 1 == 1)
    }

    /// A random ID that shares exactly the first `prefix_len` bits with `self`.
    ///
    /// When `flip_next` is true, bit `prefix_len` is inverted, so the result
    /// has a common prefix of exactly `prefix_len` bits; otherwise the common
    /// prefix is at least `prefix_len` bits.
    pub fn random_with_prefix(&self, prefix_len: usize, flip_next: bool) -> NodeId {
        let mut out: [u8; ID_LEN] = rand::random();
        let prefix_len = prefix_len.min(ID_BITS);
        for index in 0..prefix_len {
            set_bit(&mut out, index, self.bit(index));
        }
        if flip_next && prefix_len < ID_BITS {
            set_bit(&mut out, prefix_len, !self.bit(prefix_len));
        }
        NodeId(out)
    }
}

impl Distance {
    /// Number of leading zero bits (160 for a zero distance).
    pub fn leading_zeros(&self) -> usize {
        let mut total = 0usize;
        for b in self.0 {
            if b == 0 {
                total = total.saturating_add(BYTE_BITS);
            } else {
                // `leading_zeros` of a u8 is at most 8, so the cast is lossless.
                return total.saturating_add(b.leading_zeros() as usize);
            }
        }
        total
    }
}

/// Byte index and right-shift for bit `index` (0 = most significant bit).
fn bit_position(index: usize) -> (usize, u32) {
    let byte = index / BYTE_BITS;
    // `index % 8` is at most 7, so the conversion cannot fail.
    let within = u32::try_from(index % BYTE_BITS).unwrap_or(0);
    (byte, TOP_BIT_SHIFT.saturating_sub(within))
}

fn set_bit(bytes: &mut [u8; ID_LEN], index: usize, value: bool) {
    let (byte, shift) = bit_position(index);
    if let Some(b) = bytes.get_mut(byte) {
        let mask = 1u8 << shift;
        if value {
            *b |= mask;
        } else {
            *b &= !mask;
        }
    }
}

impl From<DhtKey> for NodeId {
    fn from(key: DhtKey) -> Self {
        NodeId(key.0)
    }
}

impl From<NodeId> for DhtKey {
    fn from(id: NodeId) -> Self {
        DhtKey(id.0)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", self.to_hex())
    }
}

/// Error for a node ID that is not 40 hex characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a node ID is 40 hex characters")]
pub struct NodeIdParseError;

impl FromStr for NodeId {
    type Err = NodeIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut out = [0u8; ID_LEN];
        hex::decode_to_slice(s, &mut out).map_err(|_| NodeIdParseError)?;
        Ok(NodeId(out))
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// CRC32C over the masked IP with `r = rand & 7` folded in (BEP 42 example code).
fn bep42_crc(ip: IpAddr, rand: u8) -> u32 {
    let r = (rand & BEP42_R_MASK) << BEP42_R_SHIFT;
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let mut b = [0u8; 4];
            for ((out, octet), mask) in b.iter_mut().zip(v4.octets()).zip(BEP42_V4_MASK) {
                *out = octet & mask;
            }
            b[0] |= r;
            crc32c::crc32c(&b)
        }
        IpAddr::V6(v6) => {
            let mut b = [0u8; 8];
            for ((out, octet), mask) in b.iter_mut().zip(v6.octets()).zip(BEP42_V6_MASK) {
                *out = octet & mask;
            }
            b[0] |= r;
            crc32c::crc32c(&b)
        }
    }
}

/// A BEP 42 node ID for `ip` using the given random byte (stored as the last byte).
/// The bytes the BEP leaves free are random.
pub fn bep42_id(ip: IpAddr, rand: u8) -> NodeId {
    let crc = bep42_crc(ip, rand).to_be_bytes();
    let mut id: [u8; ID_LEN] = rand::random();
    id[0] = crc[0];
    id[1] = crc[1];
    id[2] = (crc[2] & BEP42_THIRD_BYTE_MASK) | (id[2] & !BEP42_THIRD_BYTE_MASK);
    id[ID_LEN - 1] = rand;
    NodeId(id)
}

/// A fresh random BEP 42 node ID for `ip`.
pub fn bep42_random_id(ip: IpAddr) -> NodeId {
    bep42_id(ip, rand::random())
}

/// The BEP 42 `r` value of `id` (low 3 bits of the last byte).
pub fn bep42_r(id: &NodeId) -> u8 {
    id.0[ID_LEN - 1] & BEP42_R_MASK
}

/// A BEP 42 node ID for `ip` with `r` pinned (masked to 3 bits); the rest
/// of the rand byte stays random. Delegates to [`bep42_id`].
pub fn bep42_id_for_r(ip: IpAddr, r: u8) -> NodeId {
    let rand: u8 = rand::random();
    let rand = (rand & !BEP42_R_MASK) | (r & BEP42_R_MASK);
    bep42_id(ip, rand)
}

/// True when `ip` is not a public address, so BEP 42 does not apply to it
/// (loopback, private, link-local and other non-global ranges).
pub fn is_bep42_exempt(ip: IpAddr) -> bool {
    !compact::is_global_ip(ip)
}

/// True when `id` is a valid BEP 42 ID for `ip`, or `ip` is exempt.
pub fn is_bep42_valid(id: &NodeId, ip: IpAddr) -> bool {
    if is_bep42_exempt(ip) {
        return true;
    }
    let crc = bep42_crc(ip, id.0[ID_LEN - 1]).to_be_bytes();
    id.0[0] == crc[0]
        && id.0[1] == crc[1]
        && (id.0[2] & BEP42_THIRD_BYTE_MASK) == (crc[2] & BEP42_THIRD_BYTE_MASK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// (IP, rand, first three ID bytes) from BEP 42.
    const VECTORS: [(&str, u8, [u8; 3]); 5] = [
        ("124.31.75.21", 1, [0x5f, 0xbf, 0xbf]),
        ("21.75.31.124", 86, [0x5a, 0x3c, 0xe9]),
        ("65.23.51.170", 22, [0xa5, 0xd4, 0x32]),
        ("84.124.73.14", 65, [0x1b, 0x03, 0x21]),
        ("43.213.53.83", 90, [0xe5, 0x6f, 0x6c]),
    ];

    #[test]
    fn bep42_test_vectors() {
        for (ip, rand, prefix) in VECTORS {
            let ip: IpAddr = ip.parse().unwrap();
            for _ in 0..16 {
                let id = bep42_id(ip, rand);
                assert_eq!(id.0[0], prefix[0], "{ip}");
                assert_eq!(id.0[1], prefix[1], "{ip}");
                assert_eq!(id.0[2] & 0xf8, prefix[2] & 0xf8, "{ip}");
                assert_eq!(id.0[19], rand, "{ip}");
                assert!(is_bep42_valid(&id, ip));
            }
        }
    }

    #[test]
    fn bep42_r_pins_replica_ids() {
        let ip: IpAddr = "124.31.75.21".parse().unwrap();
        for r in 0..8u8 {
            let id = bep42_id_for_r(ip, r);
            assert_eq!(bep42_r(&id), r);
            assert!(is_bep42_valid(&id, ip));
        }
        // Masked to 3 bits.
        assert_eq!(bep42_r(&bep42_id_for_r(ip, 8)), 0);
    }

    #[test]
    fn bep42_validation_rejects_other_ids() {
        let ip: IpAddr = "124.31.75.21".parse().unwrap();
        let mut id = bep42_id(ip, 1);
        id.0[0] ^= 0x80;
        assert!(!is_bep42_valid(&id, ip));
        // Changing r (the last byte) changes the expected prefix.
        let mut id = bep42_id(ip, 1);
        id.0[19] = 2;
        assert!(!is_bep42_valid(&id, ip));
        // The low 3 bits of byte 2 are free.
        let mut id = bep42_id(ip, 1);
        id.0[2] ^= 0x07;
        assert!(is_bep42_valid(&id, ip));
    }

    #[test]
    fn bep42_ipv6_is_deterministic_and_valid() {
        let ip = IpAddr::V6("2001:4860:4860::8888".parse::<Ipv6Addr>().unwrap());
        let a = bep42_id(ip, 7);
        let b = bep42_id(ip, 7);
        assert_eq!(a.0[..2], b.0[..2]);
        assert!(is_bep42_valid(&a, ip));
        // Only the first 8 address bytes matter.
        let other = IpAddr::V6("2001:4860:4860:0:1:2:3:4".parse::<Ipv6Addr>().unwrap());
        assert!(is_bep42_valid(&a, other));
        let mut bad = a;
        bad.0[1] ^= 0x01;
        assert!(!is_bep42_valid(&bad, ip));
    }

    #[test]
    fn local_addresses_are_exempt() {
        let any = NodeId::random();
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "::1",
            "fe80::1",
            "fd00::1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_bep42_exempt(ip), "{ip}");
            assert!(is_bep42_valid(&any, ip));
        }
        assert!(!is_bep42_exempt(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn distance_and_prefix() {
        let a = NodeId([0u8; 20]);
        let mut b = [0u8; 20];
        b[0] = 0x80;
        let b = NodeId(b);
        assert_eq!(a.common_prefix_len(&b), 0);
        assert_eq!(a.common_prefix_len(&a), 160);
        let mut c = [0u8; 20];
        c[19] = 1;
        let c = NodeId(c);
        assert_eq!(a.common_prefix_len(&c), 159);
        assert!(a.distance(&c) < a.distance(&b));
        assert_eq!(a.distance(&b), b.distance(&a));
    }

    #[test]
    fn random_with_prefix_keeps_prefix() {
        let own = NodeId::random();
        for len in [0usize, 1, 7, 8, 9, 63, 100, 159] {
            let exact = own.random_with_prefix(len, true);
            assert_eq!(own.common_prefix_len(&exact), len);
            let atleast = own.random_with_prefix(len, false);
            assert!(own.common_prefix_len(&atleast) >= len);
        }
        assert_eq!(own.random_with_prefix(160, true), own);
    }

    #[test]
    fn hex_and_serde_round_trip() {
        let id = NodeId::random();
        assert_eq!(id.to_hex().parse::<NodeId>().unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<NodeId>(&json).unwrap(), id);
        assert!("xyz".parse::<NodeId>().is_err());
        assert_eq!(NodeId::from_slice(&[1u8; 19]), None);
        assert!(NodeId::from_slice(&[1u8; 20]).is_some());
    }
}
