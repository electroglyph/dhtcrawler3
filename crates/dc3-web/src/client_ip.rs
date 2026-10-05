//! The visitor's address, for rate limiting (`docs/03-design.md` §12).
//!
//! The address is never logged or stored.

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, HeaderName};
use ipnet::IpNet;

/// The `X-Forwarded-For` request header.
pub(crate) const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// At most this many `X-Forwarded-For` entries are examined; a longer chain
/// ends the walk as an unparseable entry would.
pub const MAX_FORWARDED_ENTRIES: usize = 64;

/// The address a request is attributed to.
///
/// The socket peer is used unless it is one of the `trusted` proxies
/// (IPv4-mapped IPv6 addresses count as IPv4). For a trusted peer, all
/// `X-Forwarded-For` header lines are joined in order and read from right to
/// left: trusted entries are skipped, and the first untrusted entry is the
/// client. An entry that does not parse as an IP address ends the walk, and
/// the last trusted hop seen (at first the peer itself) is returned; the
/// walk never continues past such an entry. When every examined entry is
/// trusted, the socket peer itself is returned: the leftmost entry, which
/// the client controls, is never preferred.
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap, trusted: &[IpNet]) -> IpAddr {
    let peer_ip = peer.ip().to_canonical();
    if !is_trusted(peer_ip, trusted) {
        return peer_ip;
    }
    let mut last_trusted = peer_ip;
    let mut examined = 0usize;
    let lines: Vec<_> = headers.get_all(X_FORWARDED_FOR).iter().collect();
    for line in lines.iter().rev() {
        let Ok(text) = line.to_str() else {
            return last_trusted;
        };
        for entry in text.rsplit(',') {
            if examined >= MAX_FORWARDED_ENTRIES {
                return last_trusted;
            }
            examined = examined.saturating_add(1);
            let Ok(ip) = entry.trim().parse::<IpAddr>() else {
                return last_trusted;
            };
            let ip = ip.to_canonical();
            if !is_trusted(ip, trusted) {
                return ip;
            }
            last_trusted = ip;
        }
    }
    // Every examined entry was trusted: attribute the request to the socket
    // peer, the only address that cannot be chosen by the sender.
    peer_ip
}

/// True when `ip` (canonical form) lies in one of `trusted`. An IPv4
/// address also matches networks written as IPv4-mapped IPv6.
fn is_trusted(ip: IpAddr, trusted: &[IpNet]) -> bool {
    let mapped = match ip {
        IpAddr::V4(v4) => Some(IpAddr::V6(v4.to_ipv6_mapped())),
        IpAddr::V6(_) => None,
    };
    trusted
        .iter()
        .any(|net| net.contains(&ip) || mapped.is_some_and(|m| net.contains(&m)))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn peer(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn xff(lines: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for line in lines {
            h.append(X_FORWARDED_FOR, HeaderValue::from_str(line).unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    const PROXY: &str = "10.0.0.1:40000";

    fn trusted() -> Vec<IpNet> {
        nets(&["10.0.0.0/8", "fd00::/8"])
    }

    #[test]
    fn untrusted_peer_is_the_client_and_the_header_is_ignored() {
        let h = xff(&["1.2.3.4"]);
        assert_eq!(
            client_ip(peer("203.0.113.9:5000"), &h, &trusted()),
            ip("203.0.113.9")
        );
        // No trusted proxies at all: the header never matters.
        assert_eq!(client_ip(peer(PROXY), &h, &[]), ip("10.0.0.1"));
    }

    #[test]
    fn trusted_peer_without_header_is_the_client() {
        assert_eq!(
            client_ip(peer(PROXY), &HeaderMap::new(), &trusted()),
            ip("10.0.0.1")
        );
    }

    #[test]
    fn single_forwarded_entry() {
        let h = xff(&["198.51.100.7"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("198.51.100.7"));
    }

    #[test]
    fn rightmost_untrusted_entry_wins_over_a_spoofed_left_entry() {
        let h = xff(&["6.6.6.6, 198.51.100.7"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("198.51.100.7"));
    }

    #[test]
    fn trusted_hops_are_skipped() {
        let h = xff(&["6.6.6.6, 198.51.100.7, 10.1.1.1, 10.2.2.2"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("198.51.100.7"));
    }

    #[test]
    fn header_lines_are_joined_in_order() {
        // Line 1 is older than line 2: the walk starts at the end of line 2.
        let h = xff(&["6.6.6.6, 198.51.100.7", "10.3.3.3"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("198.51.100.7"));
        let h = xff(&["198.51.100.7", "10.3.3.3, 10.4.4.4"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("198.51.100.7"));
    }

    #[test]
    fn unparseable_entry_stops_the_walk_at_the_last_trusted_hop() {
        let h = xff(&["198.51.100.7, garbage, 10.9.9.9"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.9.9.9"));
    }

    #[test]
    fn unparseable_rightmost_entry_gives_the_peer() {
        let h = xff(&["198.51.100.7, garbage"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.0.0.1"));
        let h = xff(&["198.51.100.7", "unknown"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.0.0.1"));
    }

    #[test]
    fn never_walks_left_past_an_unparseable_entry() {
        // Even though 198.51.100.7 would be a valid client, it lies beyond
        // the bad entry and is never used.
        let h = xff(&["198.51.100.7", "not-an-ip", "10.5.5.5"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.5.5.5"));
    }

    #[test]
    fn empty_entries_and_ports_do_not_parse() {
        let h = xff(&["198.51.100.7,,10.6.6.6"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.6.6.6"));
        let h = xff(&[""]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.0.0.1"));
        let h = xff(&["198.51.100.7:4444"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.0.0.1"));
    }

    #[test]
    fn all_trusted_chain_gives_the_socket_peer() {
        let h = xff(&["10.7.7.7, 10.8.8.8"]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.0.0.1"));
    }

    #[test]
    fn whitespace_is_trimmed() {
        let h = xff(&["  198.51.100.7 ,\t10.1.1.1 "]);
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("198.51.100.7"));
    }

    #[test]
    fn ipv6_entries_and_peers() {
        let h = xff(&["2001:db8::1, fd00::2"]);
        assert_eq!(
            client_ip(peer("[fd00::9]:443"), &h, &trusted()),
            ip("2001:db8::1")
        );
    }

    #[test]
    fn ipv4_mapped_addresses_are_canonicalised() {
        let h = xff(&["::ffff:198.51.100.7"]);
        // The mapped peer counts as the trusted IPv4 proxy.
        assert_eq!(
            client_ip(peer("[::ffff:10.0.0.1]:40000"), &h, &trusted()),
            ip("198.51.100.7")
        );
        // An untrusted mapped peer is returned as IPv4.
        assert_eq!(
            client_ip(peer("[::ffff:203.0.113.5]:1"), &h, &trusted()),
            ip("203.0.113.5")
        );
        // A trusted network written as mapped IPv6 matches IPv4 hops.
        let mapped_net = nets(&["::ffff:10.0.0.0/104"]);
        assert_eq!(client_ip(peer(PROXY), &h, &mapped_net), ip("198.51.100.7"));
    }

    #[test]
    fn non_utf8_header_line_stops_the_walk() {
        let mut h = xff(&["198.51.100.7"]);
        h.append(
            X_FORWARDED_FOR,
            HeaderValue::from_bytes(b"\xff\xfe").unwrap(),
        );
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.0.0.1"));
    }

    #[test]
    fn walk_is_bounded() {
        let chain = vec!["10.1.1.1"; MAX_FORWARDED_ENTRIES + 5].join(", ");
        let line = format!("198.51.100.7, {chain}");
        let h = xff(&[line.as_str()]);
        // The client lies beyond the examined window, so the last trusted
        // hop is used.
        assert_eq!(client_ip(peer(PROXY), &h, &trusted()), ip("10.1.1.1"));
    }
}
