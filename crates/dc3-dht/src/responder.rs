//! Answers to incoming queries (BEP 5, 32, 51), as a pure function of the
//! node's state, so the rules can be tested without sockets.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::compact::{AddrPolicy, CompactNode, Family, canonical_ip};
use crate::config::DhtTuning;
use crate::krpc::{Body, KrpcError, Message, Method, Query, Response, Want, error_code};
use crate::node_id::NodeId;
use crate::peer_store::{Announced, MAX_PEERS_PER_KEY, PeerStore};
use crate::token::TokenSecrets;
use crate::{Discovered, Source};

/// Peers drawn at random for one `get_peers` answer: all a key can hold.
/// `krpc::encode` then trims them so the datagram fits in 1 024 bytes,
/// which keeps roughly 88 IPv4 or 29 IPv6 values next to one node list.
pub(crate) const MAX_VALUES_PER_REPLY: usize = MAX_PEERS_PER_KEY;
/// Keys put in one `sample_infohashes` answer (BEP 51 suggests about 20).
pub(crate) const MAX_SAMPLES_PER_REPLY: usize = 20;

/// Everything about the request and the node that an answer depends on.
pub(crate) struct AnswerContext<'a> {
    pub(crate) own_id: NodeId,
    pub(crate) transport: Family,
    pub(crate) src: SocketAddr,
    pub(crate) now: Instant,
    pub(crate) version: [u8; 4],
    pub(crate) tuning: &'a DhtTuning,
    pub(crate) policy: AddrPolicy,
    /// Closest IPv4 nodes to the query target, if wanted and available.
    pub(crate) nodes: Option<Vec<CompactNode>>,
    /// Closest IPv6 nodes to the query target, if wanted and available.
    pub(crate) nodes6: Option<Vec<CompactNode>>,
}

pub(crate) struct Answer {
    pub(crate) reply: Message,
    pub(crate) event: Option<Discovered>,
    pub(crate) is_error: bool,
}

/// Which node families a query wants: its `want`, or the transport's family
/// when `want` is absent or names neither.
pub(crate) fn wanted(want: Option<Want>, transport: Family) -> Want {
    match want {
        Some(w) if w.n4 || w.n6 => w,
        _ => Want {
            n4: transport == Family::V4,
            n6: transport == Family::V6,
        },
    }
}

/// A KRPC error message to `ctx.src`.
pub(crate) fn error_reply(
    src: SocketAddr,
    version: [u8; 4],
    tid: &[u8],
    code: i64,
    message: &str,
) -> Message {
    Message {
        tid: tid.to_vec(),
        version: Some(version.to_vec()),
        ip: Some(src),
        read_only: false,
        body: Body::Error(KrpcError {
            code,
            message: message.to_owned(),
        }),
    }
}

fn secs_i64(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

/// Builds the answer to `query`, updating the peer store for announces.
/// `get_peers` answers carry the wanted node lists and a random sample of
/// the stored peers of the transport's family.
pub(crate) fn answer(
    ctx: &AnswerContext<'_>,
    tid: &[u8],
    query: &Query,
    tokens: &TokenSecrets,
    store: &mut PeerStore,
) -> Answer {
    let mut response = Response {
        id: ctx.own_id,
        ..Response::default()
    };
    let mut event = None;
    let from = canonical_ip(ctx.src.ip());
    let with_nodes = |r: &mut Response| {
        r.nodes = ctx.nodes.clone();
        r.nodes6 = ctx.nodes6.clone();
    };
    match &query.method {
        Method::Ping => {}
        Method::FindNode { .. } | Method::Other { .. } => with_nodes(&mut response),
        Method::GetPeers { info_hash, scrape } => {
            response.token = Some(tokens.issue(ctx.src.ip()).to_vec());
            // For scrape=1 reserve 532 B up front for BFsd/BFpe (2x256 B
            // payload + 20 B bencode keys/overhead, §5): fewer values now so
            // the filters still fit after values-first trim.
            let max_values = if *scrape {
                MAX_VALUES_PER_REPLY.min(20)
            } else {
                MAX_VALUES_PER_REPLY
            };
            let values = store.peers(info_hash, ctx.transport, max_values, ctx.now);
            if !values.is_empty() {
                response.values = Some(values);
            }
            // BEP 33: scrape=1 with local entries gains BFsd/BFpe; without
            // entries the response carries no filters (§0).
            if *scrape
                && let Some((sd, pe)) = store.filters(info_hash, ctx.transport, ctx.now)
            {
                response.bf_sd = Some(Box::new(sd.0));
                response.bf_pe = Some(Box::new(pe.0));
            }
            with_nodes(&mut response);
            event = Some(Discovered {
                key: *info_hash,
                source: Source::GetPeers,
                peer: None,
                seed: false,
                from,
            });
        }
        Method::AnnouncePeer {
            info_hash,
            port,
            implied_port,
            token,
            seed,
            ..
        } => {
            if !tokens.verify(ctx.src.ip(), token) {
                let reply =
                    error_reply(ctx.src, ctx.version, tid, error_code::PROTOCOL, "bad token");
                return Answer {
                    reply,
                    event: None,
                    is_error: true,
                };
            }
            let port = if *implied_port { ctx.src.port() } else { *port };
            let peer = SocketAddr::new(from, port);
            if !ctx.policy.dialable(&peer) {
                let reply = error_reply(
                    ctx.src,
                    ctx.version,
                    tid,
                    error_code::PROTOCOL,
                    "invalid address",
                );
                return Answer {
                    reply,
                    event: None,
                    is_error: true,
                };
            }
            // A refused announce is answered like any other, but not reported.
            if store.announce(*info_hash, peer, *seed, ctx.now) == Announced::Stored {
                event = Some(Discovered {
                    key: *info_hash,
                    source: Source::Announce,
                    peer: Some(peer),
                    seed: *seed,
                    from,
                });
            }
        }
        Method::SampleInfohashes { .. } => {
            response.samples = Some(store.sample(MAX_SAMPLES_PER_REPLY));
            response.num = Some(i64::try_from(store.len()).unwrap_or(i64::MAX));
            response.interval = Some(secs_i64(ctx.tuning.sample_interval_sent));
            with_nodes(&mut response);
        }
    }
    let reply = Message {
        tid: tid.to_vec(),
        version: Some(ctx.version.to_vec()),
        ip: Some(ctx.src),
        read_only: false,
        body: Body::Response(response),
    };
    Answer {
        reply,
        event,
        is_error: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::{OwnAddrs, is_dialable};
    use crate::krpc::{self, DecodeError, MAX_DATAGRAM_OUT};
    use crate::routing::{K, RoutingTable, TableConfig};
    use dc3_core::DhtKey;
    use proptest::prelude::*;
    use std::collections::HashSet;
    use std::net::IpAddr;
    use std::sync::Arc;

    const TTL: Duration = Duration::from_secs(45 * 60);
    const PRODUCTION: AddrPolicy = AddrPolicy {
        allow_private: false,
        by_endpoint: false,
    };
    const VERSION: [u8; 4] = *b"DC\x00\x01";

    struct Fixture {
        tuning: DhtTuning,
        tokens: TokenSecrets,
        store: PeerStore,
        table: RoutingTable,
        table6: RoutingTable,
        now: Instant,
    }

    fn table_config(family: Family) -> TableConfig {
        TableConfig {
            family,
            questionable_after: Duration::from_secs(900),
            policy: PRODUCTION,
            own: Arc::new(OwnAddrs::default()),
        }
    }

    impl Fixture {
        fn new() -> Self {
            let now = Instant::now();
            let own = NodeId([0x42; 20]);
            let mut table = RoutingTable::new(own, now, table_config(Family::V4));
            let mut table6 = RoutingTable::new(NodeId([0x24; 20]), now, table_config(Family::V6));
            for i in 0..40u32 {
                let addr = SocketAddr::from(([9, (i >> 8) as u8, i as u8, 1], 6881));
                table.on_response(NodeId::random(), addr, true, now);
                let addr6 =
                    SocketAddr::new(format!("2a00:{i:x}::1").parse::<IpAddr>().unwrap(), 6881);
                table6.on_response(NodeId::random(), addr6, true, now);
            }
            Self {
                tuning: DhtTuning::default(),
                tokens: TokenSecrets::new(now, Duration::from_secs(300)),
                store: PeerStore::new(TTL),
                table,
                table6,
                now,
            }
        }

        /// Answers `datagram` from `src` as a dual-stack node would.
        fn ask(
            &mut self,
            src: SocketAddr,
            datagram: &[u8],
        ) -> Option<(Message, Option<Discovered>)> {
            let transport = Family::of(&src);
            let (tid, query) = match krpc::decode(datagram) {
                Ok(Message {
                    tid,
                    body: Body::Query(q),
                    ..
                }) => (tid, q),
                Ok(_) => return None,
                Err(DecodeError::BadQuery { tid, code, message }) => {
                    return Some((error_reply(src, VERSION, &tid, code, message), None));
                }
                Err(_) => return None,
            };
            let want = wanted(query.want, transport);
            let target = query.method.target();
            let closest = |t: &RoutingTable| target.map(|x| t.closest(&x, K, self.now, true));
            let ctx = AnswerContext {
                own_id: self.table.own_id(),
                transport,
                src,
                now: self.now,
                version: VERSION,
                tuning: &self.tuning,
                policy: PRODUCTION,
                nodes: closest(&self.table).filter(|_| want.n4),
                nodes6: closest(&self.table6).filter(|_| want.n6),
            };
            let a = answer(&ctx, &tid, &query, &self.tokens, &mut self.store);
            Some((a.reply, a.event))
        }
    }

    fn src() -> SocketAddr {
        "8.8.8.8:4000".parse().unwrap()
    }

    fn src6() -> SocketAddr {
        "[2a01::8]:4000".parse().unwrap()
    }

    fn query_with(method: Method, want: Option<Want>) -> Vec<u8> {
        let m = Message {
            tid: b"tx".to_vec(),
            version: None,
            ip: None,
            read_only: false,
            body: Body::Query(Query {
                id: NodeId([7; 20]),
                want,
                method,
            }),
        };
        krpc::encode(&m).unwrap()
    }

    fn query(method: Method) -> Vec<u8> {
        query_with(method, None)
    }

    fn response(m: &Message) -> &Response {
        match &m.body {
            Body::Response(r) => r,
            other => panic!("expected a response, got {other:?}"),
        }
    }

    fn error_code_of(m: &Message) -> i64 {
        match &m.body {
            Body::Error(e) => e.code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn ping_and_find_node() {
        let mut f = Fixture::new();
        let (reply, event) = f.ask(src(), &query(Method::Ping)).unwrap();
        assert_eq!(reply.tid, b"tx");
        assert_eq!(reply.ip, Some(src()));
        assert_eq!(reply.version.as_deref(), Some(&VERSION[..]));
        assert_eq!(response(&reply).id, f.table.own_id());
        assert_eq!(response(&reply).nodes, None);
        assert!(event.is_none());

        // Far from [7; 20] (first bit differs), so the two answers never coincide.
        let target = NodeId([0xc3; 20]);
        let (reply, _) = f.ask(src(), &query(Method::FindNode { target })).unwrap();
        let nodes = response(&reply).nodes.clone().unwrap();
        assert_eq!(nodes, f.table.closest(&target, K, f.now, true));
        assert_eq!(nodes.len(), K);
        assert_eq!(response(&reply).nodes6, None);
        // The answer is about the target, not the sender.
        assert_ne!(nodes, f.table.closest(&NodeId([7; 20]), K, f.now, true));
        // BEP 32: `want` selects the node lists.
        let both = Some(Want { n4: true, n6: true });
        let (reply, _) = f
            .ask(src(), &query_with(Method::FindNode { target }, both))
            .unwrap();
        assert_eq!(response(&reply).nodes.as_ref().map(Vec::len), Some(K));
        assert_eq!(
            response(&reply).nodes6,
            Some(f.table6.closest(&target, K, f.now, true))
        );
        let (reply, _) = f.ask(src6(), &query(Method::FindNode { target })).unwrap();
        assert_eq!(response(&reply).nodes, None);
        assert_eq!(response(&reply).nodes6.as_ref().map(Vec::len), Some(K));
        assert_eq!(reply.ip, Some(src6()));
    }

    #[test]
    fn get_peers_and_announce() {
        let mut f = Fixture::new();
        let key = DhtKey([5; 20]);
        let from = src().ip();
        let (reply, event) = f
            .ask(src(), &query(Method::GetPeers { info_hash: key, scrape: false }))
            .unwrap();
        let r = response(&reply);
        let token = r.token.clone().unwrap();
        assert_eq!(token.len(), 8);
        assert_eq!(r.values, None);
        assert_eq!(r.nodes.as_ref().map(Vec::len), Some(K));
        assert_eq!(
            event,
            Some(Discovered {
                key,
                source: Source::GetPeers,
                peer: None,
                seed: false,
                from
            })
        );

        // Bad token: error 203 and nothing stored.
        let bad = Method::AnnouncePeer {
            info_hash: key,
            port: 7000,
            implied_port: false,
            token: vec![0; 8], seed: false };
        let (reply, event) = f.ask(src(), &query(bad)).unwrap();
        assert_eq!(error_code_of(&reply), error_code::PROTOCOL);
        assert!(event.is_none());
        assert_eq!(f.store.len(), 0);

        // A token issued to another IP is rejected too.
        let other: SocketAddr = "9.9.9.9:4000".parse().unwrap();
        let stolen = Method::AnnouncePeer {
            info_hash: key,
            port: 7000,
            implied_port: false,
            token: token.clone(), seed: false };
        assert_eq!(
            error_code_of(&f.ask(other, &query(stolen)).unwrap().0),
            error_code::PROTOCOL
        );

        // Good token with an explicit port.
        let good = Method::AnnouncePeer {
            info_hash: key,
            port: 7000,
            implied_port: false,
            token: token.clone(), seed: false };
        let (reply, event) = f.ask(src(), &query(good)).unwrap();
        assert_eq!(response(&reply).id, f.table.own_id());
        let peer: SocketAddr = "8.8.8.8:7000".parse().unwrap();
        assert_eq!(
            event,
            Some(Discovered {
                key,
                source: Source::Announce,
                peer: Some(peer),
                seed: false,
                from
            })
        );

        // implied_port uses the UDP source port.
        let implied = Method::AnnouncePeer {
            info_hash: key,
            port: 1,
            implied_port: true,
            token, seed: false };
        let (_, event) = f.ask(src(), &query(implied)).unwrap();
        assert_eq!(event.unwrap().peer, Some(src()));

        // seed=1 announces propagate the seed flag to the discovery event.
        let seeding = Method::AnnouncePeer {
            info_hash: DhtKey([6; 20]),
            port: 7001,
            implied_port: false,
            token: f.tokens.issue(src().ip()).to_vec(),
            seed: true,
        };
        let (_, event) = f.ask(src(), &query(seeding)).unwrap();
        assert!(event.unwrap().seed);

        let (reply, _) = f
            .ask(src(), &query(Method::GetPeers { info_hash: key, scrape: false }))
            .unwrap();
        // One entry per IP: the re-announce replaced the port.
        assert_eq!(response(&reply).values, Some(vec![src()]));
        // IPv6 requesters get IPv6 peers only.
        let (reply, event) = f
            .ask(src6(), &query(Method::GetPeers { info_hash: key, scrape: false }))
            .unwrap();
        assert_eq!(response(&reply).values, None);
        assert_eq!(event.unwrap().from, src6().ip());
    }

    #[test]
    fn get_peers_values_are_random_and_fill_the_datagram() {
        let mut f = Fixture::new();
        // Production keeps 100 peers per key in total; this key holds 100 of each family.
        f.store = PeerStore::with_limits(TTL, 1, 2 * MAX_PEERS_PER_KEY);
        let key = DhtKey([9; 20]);
        let v4: Vec<SocketAddr> = (0..100u16)
            .map(|i| SocketAddr::from(([8, 8, (i >> 8) as u8, i as u8], 1000 + i)))
            .collect();
        let v6: Vec<SocketAddr> = (0..100u16)
            .map(|i| {
                let ip = format!("2a02:0:0:{:x}::1", i + 1);
                SocketAddr::new(ip.parse().unwrap(), 2000 + i)
            })
            .collect();
        for peer in v4.iter().chain(&v6) {
            f.store.announce(key, *peer, false, f.now);
        }
        let all4: HashSet<SocketAddr> = v4.iter().copied().collect();
        let all6: HashSet<SocketAddr> = v6.iter().copied().collect();
        let both = Some(Want { n4: true, n6: true });
        let cases = [
            // (requester, want, stored peers of its family, size of one value, node lists)
            (src(), None, &all4, 8, (true, false)),
            (src(), both, &all4, 8, (true, true)),
            (src6(), None, &all6, 21, (false, true)),
            (src6(), both, &all6, 21, (true, true)),
        ];
        for (from, want, stored, value_len, (n4, n6)) in cases {
            let mut seen = HashSet::new();
            let mut first: Option<HashSet<SocketAddr>> = None;
            let mut differs = false;
            for _ in 0..8 {
                let (reply, _) = f
                    .ask(from, &query_with(Method::GetPeers { info_hash: key, scrape: false }, want))
                    .unwrap();
                let encoded = krpc::encode(&reply).unwrap();
                // The answer fits, and one more value would not have.
                assert!(encoded.len() <= MAX_DATAGRAM_OUT);
                assert!(
                    encoded.len() + value_len > MAX_DATAGRAM_OUT,
                    "{from} {want:?}: {} bytes",
                    encoded.len()
                );
                let back = krpc::decode(&encoded).unwrap();
                let r = response(&back);
                assert!(r.token.is_some());
                // The node lists always survive trimming.
                assert_eq!(
                    r.nodes.as_ref().map(Vec::len),
                    n4.then_some(K),
                    "{from} {want:?}"
                );
                assert_eq!(
                    r.nodes6.as_ref().map(Vec::len),
                    n6.then_some(K),
                    "{from} {want:?}"
                );
                let values: HashSet<SocketAddr> = r.values.clone().unwrap().into_iter().collect();
                assert!(
                    values.len() >= 18,
                    "{from} {want:?}: {} values",
                    values.len()
                );
                assert!(values.is_subset(stored));
                assert!(values.iter().all(|v| is_dialable(*v, false)));
                match &first {
                    None => first = Some(values.clone()),
                    Some(first) => differs |= *first != values,
                }
                seen.extend(values);
            }
            // A random sample, not the same peers every time.
            assert!(differs, "{from} {want:?}: the same values every time");
            assert!(seen.len() > first.unwrap().len());
        }
        // With one node list, about 88 IPv4 or 28 IPv6 values fit.
        let (reply, _) = f
            .ask(src(), &query(Method::GetPeers { info_hash: key, scrape: false }))
            .unwrap();
        let back = krpc::decode(&krpc::encode(&reply).unwrap()).unwrap();
        assert_eq!(response(&back).values.as_ref().map(Vec::len), Some(88));
        let (reply, _) = f
            .ask(src6(), &query(Method::GetPeers { info_hash: key, scrape: false }))
            .unwrap();
        let back = krpc::decode(&krpc::encode(&reply).unwrap()).unwrap();
        assert_eq!(response(&back).values.as_ref().map(Vec::len), Some(28));
    }

    #[test]
    fn scrape_answers_carry_filters_only_with_entries() {
        let mut f = Fixture::new();
        let key = DhtKey([7; 20]);
        // No local entries: a scrape carries no filters (§0).
        let (reply, _) = f
            .ask(
                src(),
                &query(Method::GetPeers {
                    info_hash: key,
                    scrape: true,
                }),
            )
            .unwrap();
        let r = response(&reply);
        assert_eq!(r.bf_sd, None);
        assert_eq!(r.bf_pe, None);
        // One seed + one leecher from distinct dialable IPs.
        f.store
            .announce(key, "8.8.1.1:6881".parse().unwrap(), true, f.now);
        f.store
            .announce(key, "8.8.2.2:6881".parse().unwrap(), false, f.now);
        // A plain get_peers never carries filters.
        let (reply, _) = f
            .ask(
                src(),
                &query(Method::GetPeers {
                    info_hash: key,
                    scrape: false,
                }),
            )
            .unwrap();
        let r = response(&reply);
        assert_eq!(r.bf_sd, None);
        assert_eq!(r.bf_pe, None);
        // A scrape does: both filters are 256 B and survive the wire.
        let (reply, _) = f
            .ask(
                src(),
                &query(Method::GetPeers {
                    info_hash: key,
                    scrape: true,
                }),
            )
            .unwrap();
        let r = response(&reply);
        assert_eq!(r.bf_sd.as_ref().map(|b| b.len()), Some(256));
        assert_eq!(r.bf_pe.as_ref().map(|b| b.len()), Some(256));
        assert_ne!(r.bf_sd, r.bf_pe);
        let back = krpc::decode(&krpc::encode(&reply).unwrap()).unwrap();
        let r = response(&back);
        assert_eq!(r.bf_sd.as_ref().map(|b| b.len()), Some(256));
        assert_eq!(r.bf_pe.as_ref().map(|b| b.len()), Some(256));
    }

    #[test]
    fn sample_infohashes() {
        let mut f = Fixture::new();
        let (reply, _) = f
            .ask(
                src(),
                &query(Method::SampleInfohashes {
                    target: NodeId::random(),
                }),
            )
            .unwrap();
        let r = response(&reply);
        assert_eq!(r.samples, Some(vec![]));
        assert_eq!(r.num, Some(0));
        assert_eq!(r.interval, Some(21_600));
        assert_eq!(r.nodes.as_ref().map(Vec::len), Some(K));
        for i in 0..30u8 {
            let peer = SocketAddr::from(([8, i, 4, 4], 1));
            f.store.announce(DhtKey([i; 20]), peer, false, f.now);
        }
        let (reply, _) = f
            .ask(
                src(),
                &query(Method::SampleInfohashes {
                    target: NodeId::random(),
                }),
            )
            .unwrap();
        let r = response(&reply);
        assert_eq!(
            r.samples.as_ref().map(Vec::len),
            Some(MAX_SAMPLES_PER_REPLY)
        );
        assert_eq!(r.num, Some(30));
        assert!(krpc::encode(&reply).unwrap().len() <= MAX_DATAGRAM_OUT);
    }

    #[test]
    fn unknown_methods() {
        let mut f = Fixture::new();
        let target = NodeId::random();
        let (reply, _) = f
            .ask(
                src(),
                &query(Method::Other {
                    name: b"put".to_vec(),
                    target,
                }),
            )
            .unwrap();
        assert_eq!(
            response(&reply).nodes,
            Some(f.table.closest(&target, K, f.now, true))
        );
        let (reply, _) = f
            .ask(
                src(),
                b"d1:ad2:id20:abcdefghij0123456789e1:q4:vote1:t2:aa1:y1:qe",
            )
            .unwrap();
        assert_eq!(error_code_of(&reply), error_code::METHOD_UNKNOWN);
        assert_eq!(reply.tid, b"aa");
        assert_eq!(reply.ip, Some(src()));
        let (reply, _) = f
            .ask(src(), b"d1:ad2:id3:abce1:q4:ping1:t2:aa1:y1:qe")
            .unwrap();
        assert_eq!(error_code_of(&reply), error_code::PROTOCOL);
    }

    #[test]
    fn want_rules() {
        assert_eq!(
            wanted(None, Family::V4),
            Want {
                n4: true,
                n6: false
            }
        );
        assert_eq!(
            wanted(None, Family::V6),
            Want {
                n4: false,
                n6: true
            }
        );
        assert_eq!(
            wanted(Some(Want::default()), Family::V6),
            Want {
                n4: false,
                n6: true
            }
        );
        assert_eq!(
            wanted(Some(Want { n4: true, n6: true }), Family::V4),
            Want { n4: true, n6: true }
        );
        assert_eq!(
            wanted(
                Some(Want {
                    n4: false,
                    n6: true
                }),
                Family::V4
            ),
            Want {
                n4: false,
                n6: true
            }
        );
    }

    #[test]
    fn announce_rejects_undialable_peers() {
        let mut f = Fixture::new();
        let key = DhtKey([6; 20]);
        let token = f.tokens.issue(src().ip()).to_vec();
        // implied_port with a port-0 source cannot happen on the wire, but the
        // rule is still enforced.
        let zero_src: SocketAddr = "8.8.8.8:0".parse().unwrap();
        let implied = Method::AnnouncePeer {
            info_hash: key,
            port: 0,
            implied_port: true,
            token, seed: false };
        let (reply, event) = f.ask(zero_src, &query(implied)).unwrap();
        assert_eq!(error_code_of(&reply), error_code::PROTOCOL);
        assert!(event.is_none());
        // A private sender (only possible if the caller let it through) is not stored.
        let private: SocketAddr = "192.168.1.2:5000".parse().unwrap();
        let token = f.tokens.issue(private.ip()).to_vec();
        let announce = Method::AnnouncePeer {
            info_hash: key,
            port: 7000,
            implied_port: false,
            token, seed: false };
        let (reply, event) = f.ask(private, &query(announce)).unwrap();
        assert_eq!(error_code_of(&reply), error_code::PROTOCOL);
        assert!(event.is_none());
        assert_eq!(f.store.len(), 0);
    }

    #[test]
    fn one_source_reports_a_bounded_number_of_keys() {
        let mut f = Fixture::new();
        let src = src();
        let token = f.tokens.issue(src.ip()).to_vec();
        let mut events = 0;
        for i in 0..60u8 {
            let announce = Method::AnnouncePeer {
                info_hash: DhtKey([i; 20]),
                port: 7000,
                implied_port: false,
                token: token.clone(), seed: false };
            let (reply, event) = f.ask(src, &query(announce)).unwrap();
            // Refused announces still get a normal answer.
            assert!(matches!(reply.body, Body::Response(_)));
            events += usize::from(event.is_some());
        }
        assert_eq!(events, crate::peer_store::MAX_NEW_KEYS_PER_HOST as usize);
        assert_eq!(f.store.len(), events);
    }

    #[test]
    fn mapped_senders_are_stored_as_ipv4() {
        let mut f = Fixture::new();
        let key = DhtKey([8; 20]);
        let mapped: SocketAddr = "[::ffff:8.8.8.8]:4000".parse().unwrap();
        let token = f.tokens.issue(mapped.ip()).to_vec();
        let announce = Method::AnnouncePeer {
            info_hash: key,
            port: 7000,
            implied_port: false,
            token, seed: false };
        let (_, event) = f.ask(mapped, &query(announce)).unwrap();
        let event = event.unwrap();
        assert_eq!(event.peer, Some("8.8.8.8:7000".parse().unwrap()));
        assert_eq!(event.from, "8.8.8.8".parse::<IpAddr>().unwrap());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn arbitrary_datagrams_never_panic_the_handler(bytes in proptest::collection::vec(any::<u8>(), 0..1200)) {
            let mut f = Fixture::new();
            if let Some((reply, _)) = f.ask(src(), &bytes) {
                let encoded = krpc::encode(&reply).unwrap();
                prop_assert!(encoded.len() <= MAX_DATAGRAM_OUT);
            }
        }

        #[test]
        fn structured_queries_never_panic_the_handler(
            method in 0u8..6,
            id in any::<[u8; 20]>(),
            port in any::<u16>(),
            implied in any::<bool>(),
            token in proptest::collection::vec(any::<u8>(), 0..70),
            tid in proptest::collection::vec(any::<u8>(), 1..9),
            want in proptest::option::of((any::<bool>(), any::<bool>())),
            v6 in any::<bool>(),
        ) {
            let mut f = Fixture::new();
            let key = DhtKey(id);
            let method = match method {
                0 => Method::Ping,
                1 => Method::FindNode { target: NodeId(id) },
                2 => Method::GetPeers { info_hash: key, scrape: false },
                3 => Method::AnnouncePeer { info_hash: key, port, implied_port: implied, token, seed: false },
                4 => Method::SampleInfohashes { target: NodeId(id) },
                _ => Method::Other { name: b"x".to_vec(), target: NodeId(id) },
            };
            let m = Message {
                tid,
                version: None,
                ip: None,
                read_only: false,
                body: Body::Query(Query { id: NodeId(id), want: want.map(|(n4, n6)| Want { n4, n6 }), method }),
            };
            if let Ok(bytes) = krpc::encode(&m) {
                let from: SocketAddr = if v6 { src6() } else { src() };
                if let Some((reply, _)) = f.ask(from, &bytes) {
                    prop_assert!(krpc::encode(&reply).unwrap().len() <= MAX_DATAGRAM_OUT);
                }
            }
        }
    }
}
