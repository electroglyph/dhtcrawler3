//! BEP 33 bloom filters (`BFsd` seeds, `BFpe` peers).
//!
//! Construction follows the BEP pseudocode verbatim: `m = 2048` bits
//! (`256` bytes), `k = 2`, `insertIP` via `sha1(ip)` with
//! `index1 = hash[0]|hash[1]<<8`, `index2 = hash[2]|hash[3]<<8`,
//! `index %= m`, bit `bloom[i/8] |= 1<<(i%8)`. Only IP bytes (v4 or v6)
//! are inserted; ports and host names are forbidden.
//!
//! The estimator inverts `zeros = m*(1-1/m)^(k*n)` for `n`:
//! `n = ln(zeros/m) / (k*ln(1-1/m))`. Empty (`zeros == m`) maps to `0`,
//! saturated (`zeros == 0`) maps to `None` (UNKNOWN), never `0`.

use std::net::IpAddr;

use sha1::{Digest, Sha1};

/// Bloom filter length in bytes (256 B = 2048 bits).
pub const BLOOM_LEN: usize = 256;
/// Bloom filter size in bits.
pub const BLOOM_BITS: usize = BLOOM_LEN * 8;
/// Number of hash functions.
pub const BLOOM_K: usize = 2;

/// A BEP 33 scrape bloom filter (either `BFsd` or `BFpe`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrapeBloom(pub [u8; BLOOM_LEN]);

impl ScrapeBloom {
    /// An empty filter (all zeros on the wire: no bits set).
    #[must_use]
    pub fn empty() -> Self {
        Self([0u8; BLOOM_LEN])
    }

    /// Raw bytes for KRPC encoding.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; BLOOM_LEN] {
        &self.0
    }

    /// Build from a 256-byte slice; `None` when the length differs.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != BLOOM_LEN {
            return None;
        }
        let mut arr = [0u8; BLOOM_LEN];
        arr.copy_from_slice(b);
        Some(Self(arr))
    }

    /// Insert raw IP bytes (4 for v4, 16 for v6) per BEP `insertIP`.
    pub fn insert_ip_bytes(&mut self, ip: &[u8]) {
        let mut hasher = Sha1::new();
        hasher.update(ip);
        let hash: [u8; 20] = hasher.finalize().into();
        let [b0, b1, b2, b3, ..] = hash;
        let i1 = (u16::from(b0) | (u16::from(b1) << 8)) as usize % BLOOM_BITS;
        let i2 = (u16::from(b2) | (u16::from(b3) << 8)) as usize % BLOOM_BITS;
        if let Some(slot) = self.0.get_mut(i1 / 8) {
            *slot |= 1 << (i1 % 8);
        }
        if let Some(slot) = self.0.get_mut(i2 / 8) {
            *slot |= 1 << (i2 % 8);
        }
    }

    /// Insert an IP address (v4 inserts 4 bytes, v6 inserts 16 bytes).
    pub fn insert_ip(&mut self, addr: &IpAddr) {
        match addr {
            IpAddr::V4(v4) => self.insert_ip_bytes(&v4.octets()),
            IpAddr::V6(v6) => self.insert_ip_bytes(&v6.octets()),
        }
    }

    /// Bitwise OR-union (for combining responses across nodes/families).
    pub fn union_into(&mut self, other: &Self) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a |= *b;
        }
    }

    /// OR-union of several filters; empty when the iterator is empty.
    #[must_use]
    pub fn union_all<'a>(filters: impl Iterator<Item = &'a Self>) -> Self {
        let mut out = Self::empty();
        for f in filters {
            out.union_into(f);
        }
        out
    }

    /// Number of zero bits (`m` when empty, `0` when saturated).
    #[must_use]
    pub fn zero_bits(&self) -> usize {
        self.0.iter().map(|b| b.count_zeros() as usize).sum()
    }

    /// Estimated set size, or `None` when saturated (UNKNOWN).
    /// Empty maps to `Some(0.0)`.
    #[must_use]
    pub fn estimate(&self) -> Option<f64> {
        estimate_from_zeros(self.zero_bits())
    }
}

impl Default for ScrapeBloom {
    fn default() -> Self {
        Self::empty()
    }
}

/// Estimator from a zero-bit count: `None` when saturated (`zeros == 0`),
/// `Some(0.0)` when empty (`zeros == m`), else the inversion formula.
#[must_use]
pub fn estimate_from_zeros(zeros: usize) -> Option<f64> {
    const M: f64 = BLOOM_BITS as f64;
    const K: f64 = BLOOM_K as f64;
    if zeros == 0 {
        return None;
    }
    if zeros >= BLOOM_BITS {
        return Some(0.0);
    }
    let z = zeros as f64;
    // n = ln(z/m) / (k * ln(1 - 1/m)).
    let denom = (1.0 - 1.0 / M).ln();
    Some((z / M).ln() / (K * denom))
}

/// Estimated set size of the OR-union of raw 256-byte filters, rounded
/// down. `None` when there are no filters (UNKNOWN: no aware response,
/// §3) or the union is saturated (UNKNOWN, §0). An empty union (aware
/// responses, no members of the class) estimates to `Some(0)`.
#[must_use]
pub fn estimate_or(filters: &[[u8; BLOOM_LEN]]) -> Option<u64> {
    if filters.is_empty() {
        return None;
    }
    let mut union = ScrapeBloom::empty();
    for f in filters {
        union.union_into(&ScrapeBloom(*f));
    }
    union.estimate().map(|n| n.floor() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_maps_to_zero_and_saturated_to_unknown() {
        assert_eq!(ScrapeBloom::empty().estimate(), Some(0.0));
        assert_eq!(estimate_from_zeros(BLOOM_BITS), Some(0.0));
        assert_eq!(estimate_from_zeros(0), None);
        let full = ScrapeBloom([0xFFu8; BLOOM_LEN]);
        assert_eq!(full.estimate(), None);
    }

    #[test]
    fn estimator_inverts_synthetic_inserts() {
        // ~100 distinct IPs should estimate near 100.
        let mut f = ScrapeBloom::empty();
        for i in 0..100u32 {
            let b = i.to_le_bytes();
            f.insert_ip_bytes(&b);
        }
        let est = f.estimate().expect("not saturated");
        assert!((est - 100.0).abs() < 15.0, "est {est} far from 100");
    }

    #[test]
    fn union_is_monotone() {
        let mut a = ScrapeBloom::empty();
        let mut b = ScrapeBloom::empty();
        for i in 0..50u32 {
            a.insert_ip_bytes(&i.to_le_bytes());
        }
        for i in 50..100u32 {
            b.insert_ip_bytes(&i.to_le_bytes());
        }
        let mut u = a;
        u.union_into(&b);
        assert!(u.zero_bits() <= a.zero_bits());
        assert!(u.zero_bits() <= b.zero_bits());
        let ea = a.estimate().unwrap_or(f64::INFINITY);
        let eu = u.estimate().unwrap_or(f64::INFINITY);
        assert!(eu >= ea);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(ScrapeBloom::from_bytes(&[0u8; 10]).is_none());
        assert!(ScrapeBloom::from_bytes(&[0u8; BLOOM_LEN]).is_some());
    }

    /// The BEP 33 test vector: 192.0.2.0–192.0.2.255 plus 2001:DB8::-2001:DB8::3E7
    /// (1256 addresses) must build the published filter and estimate 1224.9308.
    #[test]
    fn bep33_test_vector() {
        let mut f = ScrapeBloom::empty();
        for i in 0..=255u8 {
            f.insert_ip_bytes(&[192, 0, 2, i]);
        }
        for i in 0..=999u16 {
            let mut ip = [0u8; 16];
            ip[0] = 0x20;
            ip[1] = 0x01;
            ip[2] = 0x0d;
            ip[3] = 0xb8;
            ip[14] = (i >> 8) as u8;
            ip[15] = (i & 0xff) as u8;
            f.insert_ip_bytes(&ip);
        }
        let mut hex = String::with_capacity(2 * BLOOM_LEN);
        for b in f.0 {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02X}");
        }
        let expected = "F6C3F5EAA07FFD91BDE89F777F26FB2BFF37BDB8FB2BBAA2FD3DDDE7BACFFF75EE7CCBAE\
            FE5EEDB1FBFAFF67F6ABFF5E43DDBCA3FD9B9FFDF4FFD3E9DFF12D1BDF59DB53DBE9FA5B\
            7FF3B8FDFCDE1AFB8BEDD7BE2F3EE71EBBBFE93BCDEEFE148246C2BC5DBFF7E7EFDCF24F\
            D8DC7ADFFD8FFFDFDDFFF7A4BBEEDF5CB95CE81FC7FCFF1FF4FFFFDFE5F7FDCBB7FD79B3\
            FA1FC77BFE07FFF905B7B7FFC7FEFEFFE0B8370BB0CD3F5B7F2BD93FEB4386CFDD6F7FD5\
            BFAF2E9EBFFFFEECD67ADBF7C67F17EFD5D75EBA6FFEBA7FFF47A91EB1BFBB53E8ABFB57\
            62ABE8FF237279BFEFBFEEF5FFC5FEBFDFE5ADFFADFEE1FB737FFFFBFD9F6AEFFEEE76B6\
            FD8F72EF";
        assert_eq!(hex, expected);
        let est = f.estimate().expect("vector is not saturated");
        assert!((est - 1224.9308).abs() < 0.01, "est {est}");
    }
}
