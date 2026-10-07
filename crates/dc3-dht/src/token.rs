//! `announce_peer` tokens (BEP 5).
//!
//! A token is the first 8 bytes of SHA-1(secret ‖ IP bytes). The secret
//! rotates every few minutes and the previous one stays valid, so a token is
//! accepted for between one and two rotation periods.

use std::net::IpAddr;
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio::time::Instant;

/// Length of the tokens we issue.
pub(crate) const TOKEN_LEN: usize = 8;
/// Length of a token secret.
const SECRET_LEN: usize = 20;

pub(crate) struct TokenSecrets {
    current: [u8; SECRET_LEN],
    previous: [u8; SECRET_LEN],
    rotated_at: Instant,
    interval: Duration,
}

impl TokenSecrets {
    pub(crate) fn new(now: Instant, interval: Duration) -> Self {
        Self {
            current: rand::random(),
            previous: rand::random(),
            rotated_at: now,
            interval,
        }
    }

    /// Rotates the secret if the interval has passed. Returns true if it rotated.
    pub(crate) fn maybe_rotate(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.rotated_at) < self.interval {
            return false;
        }
        self.previous = self.current;
        self.current = rand::random();
        self.rotated_at = now;
        true
    }

    /// The token for requests from `ip`.
    pub(crate) fn issue(&self, ip: IpAddr) -> [u8; TOKEN_LEN] {
        compute(&self.current, ip)
    }

    /// Whether `token` was issued to `ip` under the current or previous secret.
    pub(crate) fn verify(&self, ip: IpAddr, token: &[u8]) -> bool {
        // Evaluate both comparisons so timing does not reveal which one matched.
        let current = constant_time_eq(token, &compute(&self.current, ip));
        let previous = constant_time_eq(token, &compute(&self.previous, ip));
        current | previous
    }
}

fn compute(secret: &[u8; SECRET_LEN], ip: IpAddr) -> [u8; TOKEN_LEN] {
    let mut hasher = Sha1::new();
    hasher.update(secret);
    match ip.to_canonical() {
        IpAddr::V4(v4) => hasher.update(v4.octets()),
        IpAddr::V6(v6) => hasher.update(v6.octets()),
    }
    let digest = hasher.finalize();
    let mut out = [0u8; TOKEN_LEN];
    out.copy_from_slice(&digest[..TOKEN_LEN]);
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIVE_MIN: Duration = Duration::from_secs(300);

    #[test]
    fn tokens_are_per_ip() {
        let t0 = Instant::now();
        let s = TokenSecrets::new(t0, FIVE_MIN);
        let a: IpAddr = "1.2.3.4".parse().unwrap();
        let b: IpAddr = "1.2.3.5".parse().unwrap();
        let tok = s.issue(a);
        assert_eq!(tok.len(), TOKEN_LEN);
        assert!(s.verify(a, &tok));
        assert!(!s.verify(b, &tok));
        assert!(!s.verify(a, &tok[..7]));
        assert!(!s.verify(a, b""));
        // IPv4-mapped IPv6 is the same IP.
        assert!(s.verify("::ffff:1.2.3.4".parse().unwrap(), &tok));
    }

    #[test]
    fn token_is_sha1_prefix() {
        use sha1::{Digest, Sha1};

        let t0 = Instant::now();
        let s = TokenSecrets::new(t0, FIVE_MIN);
        for ip in [
            "1.2.3.4".parse().unwrap(),
            "2a00::1".parse().unwrap(),
            "::ffff:1.2.3.4".parse().unwrap(),
        ] {
            let ip: IpAddr = ip;
            let mut h = Sha1::new();
            h.update(s.current);
            match ip.to_canonical() {
                IpAddr::V4(v4) => h.update(v4.octets()),
                IpAddr::V6(v6) => h.update(v6.octets()),
            }
            let digest = h.finalize();
            let expected = <[u8; TOKEN_LEN]>::try_from(&digest[..TOKEN_LEN]).unwrap();
            assert_eq!(s.issue(ip), expected);
        }
    }

    #[test]
    fn rotation_window() {
        let t0 = Instant::now();
        let mut s = TokenSecrets::new(t0, FIVE_MIN);
        let ip: IpAddr = "2a00::1".parse().unwrap();
        let tok = s.issue(ip);
        assert!(!s.maybe_rotate(t0 + Duration::from_secs(299)));
        assert!(s.verify(ip, &tok));
        // First rotation: the token is still accepted as "previous".
        assert!(s.maybe_rotate(t0 + FIVE_MIN));
        assert!(s.verify(ip, &tok));
        assert_ne!(s.issue(ip), tok);
        // Second rotation: it expires.
        assert!(!s.maybe_rotate(t0 + FIVE_MIN + Duration::from_secs(10)));
        assert!(s.maybe_rotate(t0 + FIVE_MIN * 2));
        assert!(!s.verify(ip, &tok));
    }

    #[test]
    fn distinct_secrets_give_distinct_tokens() {
        let t0 = Instant::now();
        let a = TokenSecrets::new(t0, FIVE_MIN);
        let b = TokenSecrets::new(t0, FIVE_MIN);
        let ip: IpAddr = "8.8.8.8".parse().unwrap();
        assert_ne!(a.issue(ip), b.issue(ip));
        assert!(!b.verify(ip, &a.issue(ip)));
    }
}
