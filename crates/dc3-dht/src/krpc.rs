//! KRPC messages (BEP 5, 32, 43, 51): strict decoding and bounded encoding.
//!
//! [`decode`] turns one datagram into a typed [`Message`]. It never panics.
//! [`DecodeError`] says whether the datagram should be dropped silently or
//! answered with a KRPC error. [`encode`] always produces at most
//! [`MAX_DATAGRAM_OUT`] bytes, trimming `values`, `samples`, `nodes6` and
//! `nodes` (in that order) when a response would be too large.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use dc3_bencode::{Dict, Limits, OwnedValue, Value};
use dc3_core::DhtKey;

use crate::compact::{self, CompactNode, Family};
use crate::node_id::{ID_LEN, NodeId};

/// Largest datagram we send (BEP 32).
pub const MAX_DATAGRAM_OUT: usize = 1024;
/// Longest transaction ID we accept.
pub const MAX_TID_LEN: usize = 8;
/// Longest `token` we accept.
pub const MAX_TOKEN_LEN: usize = 64;
/// Longest `v` (client version) we keep; longer values are ignored.
pub const MAX_VERSION_LEN: usize = 16;
/// Longest method name kept for an unknown query.
pub const MAX_METHOD_NAME_LEN: usize = 64;
/// Longest error text kept from a KRPC error.
pub const MAX_ERROR_TEXT_LEN: usize = 256;

/// KRPC error codes (BEP 5).
pub mod error_code {
    /// Generic error.
    pub const GENERIC: i64 = 201;
    /// Server error.
    pub const SERVER: i64 = 202;
    /// Protocol error: malformed packet, invalid arguments or bad token.
    pub const PROTOCOL: i64 = 203;
    /// Method unknown.
    pub const METHOD_UNKNOWN: i64 = 204;
}

/// The `want` argument (BEP 32).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Want {
    pub n4: bool,
    pub n6: bool,
}

/// One KRPC message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// Transaction ID, 1..=8 bytes.
    pub tid: Vec<u8>,
    /// Client version (`v`), if present and short.
    pub version: Option<Vec<u8>>,
    /// Top-level `ip`: the endpoint the sender saw us at (BEP 42).
    pub ip: Option<SocketAddr>,
    /// Top-level `ro` = 1 (BEP 43): the sender is read-only.
    pub read_only: bool,
    pub body: Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Query(Query),
    Response(Response),
    Error(KrpcError),
}

/// A query (`y` = `q`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// The sender's node ID (`a.id`).
    pub id: NodeId,
    /// `a.want`; `None` when absent.
    pub want: Option<Want>,
    pub method: Method,
}

/// A query method and its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Ping,
    FindNode {
        target: NodeId,
    },
    GetPeers {
        info_hash: DhtKey,
        /// BEP 33 `scrape=1`: ask for `BFsd`/`BFpe` filters.
        scrape: bool,
    },
    /// `port` is meaningful only when `implied_port` is false.
    AnnouncePeer {
        info_hash: DhtKey,
        port: u16,
        implied_port: bool,
        token: Vec<u8>,
        /// BEP 33 `seed=1`: the announcer is a seed. Missing/`!=1` means peer.
        seed: bool,
    },
    SampleInfohashes {
        target: NodeId,
    },
    /// An unknown method carrying a 20-byte `target` or `info_hash`; it is
    /// answered like `find_node`.
    Other {
        name: Vec<u8>,
        target: NodeId,
    },
}

impl Method {
    /// The method name as sent on the wire.
    pub fn name(&self) -> &[u8] {
        match self {
            Method::Ping => b"ping",
            Method::FindNode { .. } => b"find_node",
            Method::GetPeers { .. } => b"get_peers",
            Method::AnnouncePeer { .. } => b"announce_peer",
            Method::SampleInfohashes { .. } => b"sample_infohashes",
            Method::Other { name, .. } => name,
        }
    }

    /// The ID the query is about, if any (used to pick the closest nodes).
    pub fn target(&self) -> Option<NodeId> {
        match self {
            Method::Ping | Method::AnnouncePeer { .. } => None,
            Method::FindNode { target }
            | Method::SampleInfohashes { target }
            | Method::Other { target, .. } => Some(*target),
            Method::GetPeers { info_hash, .. } => Some(NodeId::from(*info_hash)),
        }
    }
}

/// A response (`y` = `r`). `None` means the key was absent.
/// `bf_sd`/`bf_pe` are BEP 33 scrape filters (256 B each); `None` means
/// absent (responder has no entries, or the query was not a scrape).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub id: NodeId,
    /// IPv4 nodes. A list with a bad length decodes as `None`.
    pub nodes: Option<Vec<CompactNode>>,
    /// IPv6 nodes. A list with a bad length decodes as `None`.
    pub nodes6: Option<Vec<CompactNode>>,
    pub token: Option<Vec<u8>>,
    /// Peer endpoints (entries with bad lengths are skipped).
    pub values: Option<Vec<SocketAddr>>,
    /// BEP 51 samples. A string whose length is not a multiple of 20 decodes as `None`.
    pub samples: Option<Vec<DhtKey>>,
    pub num: Option<i64>,
    pub interval: Option<i64>,
    /// BEP 33 seeds filter (`BFsd`), exactly 256 bytes on the wire. Boxed:
    /// two inline 256-byte arrays would push `Body` over the
    /// large-enum-variant threshold.
    pub bf_sd: Option<Box<[u8; crate::bloom::BLOOM_LEN]>>,
    /// BEP 33 peers filter (`BFpe`), exactly 256 bytes on the wire.
    pub bf_pe: Option<Box<[u8; crate::bloom::BLOOM_LEN]>>,
}

/// A KRPC error (`y` = `e`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KrpcError {
    pub code: i64,
    pub message: String,
}

/// Why a datagram could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// Not usable at all (bad bencode, no usable `t` or `y`): drop silently.
    #[error("unusable datagram")]
    Unusable,
    /// A query that should be answered with a KRPC error.
    #[error("bad query ({code}): {message}")]
    BadQuery {
        tid: Vec<u8>,
        code: i64,
        message: &'static str,
    },
    /// A response or error whose body is malformed.
    #[error("malformed reply")]
    BadReply { tid: Vec<u8> },
}

/// Why a message could not be encoded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EncodeError {
    #[error("KRPC message does not fit in {MAX_DATAGRAM_OUT} bytes")]
    TooLarge,
}

type BadArg = (i64, &'static str);

/// Decodes one datagram.
pub fn decode(buf: &[u8]) -> Result<Message, DecodeError> {
    let value = dc3_bencode::decode(buf, &Limits::KRPC).map_err(|_| DecodeError::Unusable)?;
    let top = value.as_dict().ok_or(DecodeError::Unusable)?;
    let tid = top
        .get_bytes(b"t")
        .filter(|t| (1..=MAX_TID_LEN).contains(&t.len()))
        .ok_or(DecodeError::Unusable)?
        .to_vec();
    let y = top.get_bytes(b"y").ok_or(DecodeError::Unusable)?;
    let version = top
        .get_bytes(b"v")
        .filter(|v| v.len() <= MAX_VERSION_LEN)
        .map(<[u8]>::to_vec);
    let ip = top.get_bytes(b"ip").and_then(compact::decode_bep42_ip);
    let read_only = top.get_int(b"ro") == Some(1);
    let body = match y {
        b"q" => match decode_query(top) {
            Ok(q) => Body::Query(q),
            Err((code, message)) => return Err(DecodeError::BadQuery { tid, code, message }),
        },
        b"r" => match decode_response(top) {
            Some(r) => Body::Response(r),
            None => return Err(DecodeError::BadReply { tid }),
        },
        b"e" => match decode_error(top) {
            Some(e) => Body::Error(e),
            None => return Err(DecodeError::BadReply { tid }),
        },
        _ => return Err(DecodeError::Unusable),
    };
    Ok(Message {
        tid,
        version,
        ip,
        read_only,
        body,
    })
}

fn id_arg(args: &Dict<'_>, key: &[u8], missing: &'static str) -> Result<NodeId, BadArg> {
    args.get_bytes(key)
        .and_then(NodeId::from_slice)
        .ok_or((error_code::PROTOCOL, missing))
}

fn decode_query(top: &Dict<'_>) -> Result<Query, BadArg> {
    let name = top
        .get_bytes(b"q")
        .ok_or((error_code::PROTOCOL, "missing or invalid 'q'"))?;
    let args = top
        .get_dict(b"a")
        .ok_or((error_code::PROTOCOL, "missing or invalid 'a'"))?;
    let id = id_arg(args, b"id", "missing or invalid 'id'")?;
    let want = match args.get(b"want") {
        None => None,
        Some(v) => Some(decode_want(v)?),
    };
    let method = match name {
        b"ping" => Method::Ping,
        b"find_node" => Method::FindNode {
            target: id_arg(args, b"target", "missing or invalid 'target'")?,
        },
        b"get_peers" => Method::GetPeers {
            info_hash: DhtKey::from(id_arg(
                args,
                b"info_hash",
                "missing or invalid 'info_hash'",
            )?),
            scrape: args.get_int(b"scrape") == Some(1),
        },
        b"announce_peer" => decode_announce(args)?,
        b"sample_infohashes" => Method::SampleInfohashes {
            target: id_arg(args, b"target", "missing or invalid 'target'")?,
        },
        other => {
            // libtorrent's forward-compatibility rule: an unknown method with a
            // usable target or info_hash is treated as find_node.
            let target = args
                .get_bytes(b"target")
                .and_then(NodeId::from_slice)
                .or_else(|| args.get_bytes(b"info_hash").and_then(NodeId::from_slice))
                .ok_or((error_code::METHOD_UNKNOWN, "method unknown"))?;
            let name = other.get(..MAX_METHOD_NAME_LEN).unwrap_or(other).to_vec();
            Method::Other { name, target }
        }
    };
    Ok(Query { id, want, method })
}

fn decode_announce(args: &Dict<'_>) -> Result<Method, BadArg> {
    let info_hash = DhtKey::from(id_arg(
        args,
        b"info_hash",
        "missing or invalid 'info_hash'",
    )?);
    let implied_port = args.get_int(b"implied_port") == Some(1);
    let port = args
        .get_int(b"port")
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0);
    let port = match (implied_port, port) {
        (_, Some(p)) => p,
        (true, None) => 0,
        (false, None) => return Err((error_code::PROTOCOL, "missing or invalid 'port'")),
    };
    let token = args
        .get_bytes(b"token")
        .filter(|t| t.len() <= MAX_TOKEN_LEN)
        .ok_or((error_code::PROTOCOL, "missing or invalid 'token'"))?
        .to_vec();
    Ok(Method::AnnouncePeer {
        info_hash,
        port,
        implied_port,
        token,
        seed: args.get_int(b"seed") == Some(1),
    })
}

fn decode_want(v: &Value<'_>) -> Result<Want, BadArg> {
    const BAD: BadArg = (error_code::PROTOCOL, "invalid 'want'");
    let mut want = Want::default();
    for item in v.as_list().ok_or(BAD)? {
        match item.as_bytes().ok_or(BAD)? {
            b"n4" => want.n4 = true,
            b"n6" => want.n6 = true,
            _ => {}
        }
    }
    Ok(want)
}

fn decode_response(top: &Dict<'_>) -> Option<Response> {
    let r = top.get_dict(b"r")?;
    let id = r.get_bytes(b"id").and_then(NodeId::from_slice)?;
    let bf_sd = r.get_bytes(b"BFsd").and_then(|b| {
        if b.len() == crate::bloom::BLOOM_LEN {
            let mut arr = [0u8; crate::bloom::BLOOM_LEN];
            arr.copy_from_slice(b);
            Some(Box::new(arr))
        } else {
            None
        }
    });
    let bf_pe = r.get_bytes(b"BFpe").and_then(|b| {
        if b.len() == crate::bloom::BLOOM_LEN {
            let mut arr = [0u8; crate::bloom::BLOOM_LEN];
            arr.copy_from_slice(b);
            Some(Box::new(arr))
        } else {
            None
        }
    });
    Some(Response {
        id,
        nodes: r
            .get_bytes(b"nodes")
            .and_then(|b| compact::decode_nodes(b, Family::V4)),
        nodes6: r
            .get_bytes(b"nodes6")
            .and_then(|b| compact::decode_nodes(b, Family::V6)),
        token: r
            .get_bytes(b"token")
            .filter(|t| t.len() <= MAX_TOKEN_LEN)
            .map(<[u8]>::to_vec),
        values: r.get_list(b"values").map(decode_values),
        samples: r.get_bytes(b"samples").and_then(decode_samples),
        num: r.get_int(b"num"),
        interval: r.get_int(b"interval"),
        bf_sd,
        bf_pe,
    })
}

/// Peer values: one compact endpoint per string. A single string holding
/// several concatenated IPv4 endpoints (old Mainline format) is also accepted,
/// but only when its length is unambiguous: a multiple of 6 that is not also
/// a multiple of 18. A length that is a multiple of 18 could equally be
/// concatenated 18-byte IPv6 endpoints, which no specification defines; those
/// fall through to the per-element path (which drops the string) rather than
/// fabricating IPv4 peers out of IPv6 bytes.
fn decode_values(list: &[Value<'_>]) -> Vec<SocketAddr> {
    if let [only] = list
        && let Some(b) = only.as_bytes()
        && b.len() != compact::COMPACT_PEER_V4_LEN
        && b.len() != compact::COMPACT_PEER_V6_LEN
        && b.len().is_multiple_of(compact::COMPACT_PEER_V4_LEN)
        && !b.len().is_multiple_of(compact::COMPACT_PEER_V6_LEN)
    {
        let (chunks, _) = b.as_chunks::<{ compact::COMPACT_PEER_V4_LEN }>();
        return chunks
            .iter()
            .filter_map(|c| compact::decode_peer(c))
            .collect();
    }
    list.iter()
        .filter_map(|v| v.as_bytes().and_then(compact::decode_peer))
        .collect()
}

fn decode_samples(bytes: &[u8]) -> Option<Vec<DhtKey>> {
    if !bytes.len().is_multiple_of(ID_LEN) {
        return None;
    }
    let (chunks, _) = bytes.as_chunks::<ID_LEN>();
    Some(chunks.iter().map(|c| DhtKey(*c)).collect())
}

fn decode_error(top: &Dict<'_>) -> Option<KrpcError> {
    let e = top.get_list(b"e")?;
    let code = e.first()?.as_int()?;
    let message = e
        .get(1)
        .and_then(Value::as_bytes)
        .map(|b| String::from_utf8_lossy(b.get(..MAX_ERROR_TEXT_LEN).unwrap_or(b)).into_owned())
        .unwrap_or_default();
    Some(KrpcError { code, message })
}

fn key(k: &[u8]) -> Vec<u8> {
    k.to_vec()
}

fn to_value(msg: &Message) -> OwnedValue {
    let mut top: BTreeMap<Vec<u8>, OwnedValue> = OwnedValue::dict();
    top.insert(key(b"t"), OwnedValue::bytes(msg.tid.clone()));
    if let Some(v) = &msg.version {
        top.insert(key(b"v"), OwnedValue::bytes(v.clone()));
    }
    if let Some(ip) = &msg.ip {
        top.insert(key(b"ip"), OwnedValue::bytes(compact::encode_peer(ip)));
    }
    if msg.read_only {
        top.insert(key(b"ro"), OwnedValue::Int(1));
    }
    match &msg.body {
        Body::Query(q) => {
            top.insert(key(b"y"), OwnedValue::from("q"));
            top.insert(key(b"q"), OwnedValue::bytes(q.method.name().to_vec()));
            top.insert(key(b"a"), OwnedValue::Dict(query_args(q)));
        }
        Body::Response(r) => {
            top.insert(key(b"y"), OwnedValue::from("r"));
            top.insert(key(b"r"), OwnedValue::Dict(response_fields(r)));
        }
        Body::Error(e) => {
            top.insert(key(b"y"), OwnedValue::from("e"));
            top.insert(
                key(b"e"),
                OwnedValue::List(vec![
                    OwnedValue::Int(e.code),
                    OwnedValue::from(e.message.as_str()),
                ]),
            );
        }
    }
    OwnedValue::Dict(top)
}

fn query_args(q: &Query) -> BTreeMap<Vec<u8>, OwnedValue> {
    let mut a = OwnedValue::dict();
    a.insert(key(b"id"), OwnedValue::bytes(q.id.0.to_vec()));
    if let Some(w) = q.want {
        let mut list = Vec::new();
        if w.n4 {
            list.push(OwnedValue::from("n4"));
        }
        if w.n6 {
            list.push(OwnedValue::from("n6"));
        }
        a.insert(key(b"want"), OwnedValue::List(list));
    }
    match &q.method {
        Method::Ping => {}
        Method::FindNode { target }
        | Method::SampleInfohashes { target }
        | Method::Other { target, .. } => {
            a.insert(key(b"target"), OwnedValue::bytes(target.0.to_vec()));
        }
        Method::GetPeers { info_hash, scrape } => {
            a.insert(key(b"info_hash"), OwnedValue::bytes(info_hash.0.to_vec()));
            if *scrape {
                a.insert(key(b"scrape"), OwnedValue::Int(1));
            }
        }
        Method::AnnouncePeer {
            info_hash,
            port,
            implied_port,
            token,
            seed,
        } => {
            a.insert(key(b"info_hash"), OwnedValue::bytes(info_hash.0.to_vec()));
            a.insert(key(b"port"), OwnedValue::Int(i64::from(*port)));
            a.insert(
                key(b"implied_port"),
                OwnedValue::Int(i64::from(*implied_port)),
            );
            a.insert(key(b"token"), OwnedValue::bytes(token.clone()));
            if *seed {
                a.insert(key(b"seed"), OwnedValue::Int(1));
            }
        }
    }
    a
}

fn response_fields(r: &Response) -> BTreeMap<Vec<u8>, OwnedValue> {
    let mut d = OwnedValue::dict();
    d.insert(key(b"id"), OwnedValue::bytes(r.id.0.to_vec()));
    if let Some(nodes) = &r.nodes {
        d.insert(
            key(b"nodes"),
            OwnedValue::bytes(compact::encode_nodes(nodes, Family::V4)),
        );
    }
    if let Some(nodes) = &r.nodes6 {
        d.insert(
            key(b"nodes6"),
            OwnedValue::bytes(compact::encode_nodes(nodes, Family::V6)),
        );
    }
    if let Some(token) = &r.token {
        d.insert(key(b"token"), OwnedValue::bytes(token.clone()));
    }
    if let Some(values) = &r.values {
        let list = values
            .iter()
            .map(|v| OwnedValue::bytes(compact::encode_peer(v)))
            .collect();
        d.insert(key(b"values"), OwnedValue::List(list));
    }
    if let Some(samples) = &r.samples {
        let mut bytes = Vec::with_capacity(samples.len().saturating_mul(ID_LEN));
        for s in samples {
            bytes.extend_from_slice(&s.0);
        }
        d.insert(key(b"samples"), OwnedValue::bytes(bytes));
    }
    if let Some(num) = r.num {
        d.insert(key(b"num"), OwnedValue::Int(num));
    }
    if let Some(interval) = r.interval {
        d.insert(key(b"interval"), OwnedValue::Int(interval));
    }
    if let Some(bf) = &r.bf_sd {
        d.insert(key(b"BFsd"), OwnedValue::bytes(bf.to_vec()));
    }
    if let Some(bf) = &r.bf_pe {
        d.insert(key(b"BFpe"), OwnedValue::bytes(bf.to_vec()));
    }
    d
}

/// Encoded size of one compact peer inside a bencoded list (`6:…` or `18:…`).
fn value_entry_len(addr: &SocketAddr) -> usize {
    match addr {
        SocketAddr::V4(_) => compact::COMPACT_PEER_V4_LEN.saturating_add(2),
        SocketAddr::V6(_) => compact::COMPACT_PEER_V6_LEN.saturating_add(3),
    }
}

/// Pops entries from the end of `list` until at least `excess` bytes are gone.
fn pop_bytes<T>(list: &mut Vec<T>, excess: usize, size: impl Fn(&T) -> usize) {
    let mut removed = 0usize;
    while removed < excess {
        match list.pop() {
            Some(item) => removed = removed.saturating_add(size(&item)),
            None => break,
        }
    }
}

/// Removes entries worth at least `excess` bytes from the first non-empty
/// trimmable list. Returns false when nothing is left to trim.
fn trim(r: &mut Response, excess: usize) -> bool {
    if let Some(v) = r.values.as_mut().filter(|v| !v.is_empty()) {
        pop_bytes(v, excess, value_entry_len);
    } else if let Some(s) = r.samples.as_mut().filter(|s| !s.is_empty()) {
        pop_bytes(s, excess, |_| ID_LEN);
    } else if let Some(n) = r.nodes6.as_mut().filter(|n| !n.is_empty()) {
        pop_bytes(n, excess, |_| compact::COMPACT_NODE_V6_LEN);
    } else if let Some(n) = r.nodes.as_mut().filter(|n| !n.is_empty()) {
        pop_bytes(n, excess, |_| compact::COMPACT_NODE_V4_LEN);
    } else {
        return false;
    }
    true
}

/// Encodes `msg`, trimming response lists so the result fits in
/// [`MAX_DATAGRAM_OUT`] bytes.
pub fn encode(msg: &Message) -> Result<Vec<u8>, EncodeError> {
    let bytes = dc3_bencode::encode(&to_value(msg));
    if bytes.len() <= MAX_DATAGRAM_OUT {
        return Ok(bytes);
    }
    let mut msg = msg.clone();
    let mut len = bytes.len();
    loop {
        let excess = len.saturating_sub(MAX_DATAGRAM_OUT);
        let trimmed = match &mut msg.body {
            Body::Response(r) => trim(r, excess),
            Body::Query(_) | Body::Error(_) => false,
        };
        if !trimmed {
            return Err(EncodeError::TooLarge);
        }
        let bytes = dc3_bencode::encode(&to_value(&msg));
        if bytes.len() <= MAX_DATAGRAM_OUT {
            return Ok(bytes);
        }
        len = bytes.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn msg(body: Body) -> Message {
        Message {
            tid: b"aa".to_vec(),
            version: Some(b"DC\x00\x01".to_vec()),
            ip: Some(sa("1.2.3.4:5")),
            read_only: false,
            body,
        }
    }

    fn round_trip(m: &Message) {
        let bytes = encode(m).unwrap();
        assert!(bytes.len() <= MAX_DATAGRAM_OUT);
        assert_eq!(&decode(&bytes).unwrap(), m);
    }

    #[test]
    fn bep5_ping_examples() {
        let q = decode(b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe").unwrap();
        assert_eq!(q.tid, b"aa");
        assert!(!q.read_only);
        let Body::Query(query) = &q.body else {
            panic!("not a query")
        };
        assert_eq!(query.id.0, *b"abcdefghij0123456789");
        assert_eq!(query.method, Method::Ping);
        assert_eq!(query.want, None);
        assert_eq!(
            encode(&q).unwrap(),
            b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe"
        );

        let r = decode(b"d1:rd2:id20:mnopqrstuvwxyz123456e1:t2:aa1:y1:re").unwrap();
        let Body::Response(resp) = &r.body else {
            panic!("not a response")
        };
        assert_eq!(resp.id.0, *b"mnopqrstuvwxyz123456");
        assert_eq!(resp.nodes, None);
        assert_eq!(
            encode(&r).unwrap(),
            b"d1:rd2:id20:mnopqrstuvwxyz123456e1:t2:aa1:y1:re"
        );
    }

    #[test]
    fn bep5_error_example() {
        let e = decode(b"d1:eli201e23:A Generic Error Ocurrede1:t2:aa1:y1:ee").unwrap();
        assert_eq!(
            e.body,
            Body::Error(KrpcError {
                code: 201,
                message: "A Generic Error Ocurred".into()
            })
        );
        assert_eq!(
            encode(&e).unwrap(),
            b"d1:eli201e23:A Generic Error Ocurrede1:t2:aa1:y1:ee"
        );
    }

    #[test]
    fn query_round_trips() {
        let id = NodeId([1; 20]);
        let key = DhtKey([2; 20]);
        let methods = [
            Method::Ping,
            Method::FindNode {
                target: NodeId([3; 20]),
            },
            Method::GetPeers {
                info_hash: key,
                scrape: false,
            },
            Method::AnnouncePeer {
                info_hash: key,
                port: 6881,
                implied_port: false,
                token: b"tok".to_vec(),
                seed: false,
            },
            Method::AnnouncePeer {
                info_hash: key,
                port: 0,
                implied_port: true,
                token: vec![0; 8],
                seed: false,
            },
            Method::SampleInfohashes {
                target: NodeId([4; 20]),
            },
            Method::Other {
                name: b"vote".to_vec(),
                target: NodeId([5; 20]),
            },
        ];
        for method in methods {
            for want in [
                None,
                Some(Want { n4: true, n6: true }),
                Some(Want {
                    n4: false,
                    n6: true,
                }),
            ] {
                let mut m = msg(Body::Query(Query {
                    id,
                    want,
                    method: method.clone(),
                }));
                round_trip(&m);
                m.read_only = true;
                m.ip = None;
                m.version = None;
                round_trip(&m);
            }
        }
    }

    #[test]
    fn bep33_scrape_seed_and_filters_round_trip() {
        let id = NodeId([1; 20]);
        let key = DhtKey([2; 20]);
        // scrape=1 / seed=1 survive a round trip; missing means false.
        let q = msg(Body::Query(Query {
            id,
            want: None,
            method: Method::GetPeers {
                info_hash: key,
                scrape: true,
            },
        }));
        let back = decode(&encode(&q).unwrap()).unwrap();
        let Body::Query(q2) = back.body else {
            panic!("not a query")
        };
        assert_eq!(
            q2.method,
            Method::GetPeers {
                info_hash: key,
                scrape: true,
            }
        );
        let q = msg(Body::Query(Query {
            id,
            want: None,
            method: Method::AnnouncePeer {
                info_hash: key,
                port: 6881,
                implied_port: false,
                token: b"tok".to_vec(),
                seed: true,
            },
        }));
        let back = decode(&encode(&q).unwrap()).unwrap();
        let Body::Query(q2) = back.body else {
            panic!("not a query")
        };
        assert!(matches!(q2.method, Method::AnnouncePeer { seed: true, .. }));
        // Filters require exactly 256 B; other lengths decode as absent.
        let mut r = Response {
            id,
            bf_sd: Some(Box::new([7u8; crate::bloom::BLOOM_LEN])),
            bf_pe: Some(Box::new([9u8; crate::bloom::BLOOM_LEN])),
            ..Response::default()
        };
        round_trip(&msg(Body::Response(r.clone())));
        r.bf_sd = None;
        r.bf_pe = None;
        round_trip(&msg(Body::Response(r)));
    }

    #[test]
    fn response_and_error_round_trips() {
        let nodes = vec![CompactNode {
            id: NodeId([9; 20]),
            addr: sa("9.9.9.9:99"),
        }];
        let nodes6 = vec![CompactNode {
            id: NodeId([8; 20]),
            addr: sa("[2a00::8]:88"),
        }];
        let full = Response {
            id: NodeId([7; 20]),
            nodes: Some(nodes),
            nodes6: Some(nodes6),
            token: Some(vec![1, 2, 3, 4, 5, 6, 7, 8]),
            values: Some(vec![sa("5.5.5.5:55"), sa("[2a00::5]:55")]),
            samples: Some(vec![DhtKey([1; 20]), DhtKey([2; 20])]),
            num: Some(2),
            interval: Some(21600),
            ..Response::default()
        };
        round_trip(&msg(Body::Response(full)));
        round_trip(&msg(Body::Response(Response {
            id: NodeId([7; 20]),
            nodes: Some(vec![]),
            ..Response::default()
        })));
        round_trip(&msg(Body::Error(KrpcError {
            code: 203,
            message: "bad token".into(),
        })));
        round_trip(&msg(Body::Error(KrpcError {
            code: 204,
            message: String::new(),
        })));
    }

    #[test]
    fn top_level_fields() {
        let m = decode(b"d2:ip6:\x01\x02\x03\x04\x00\x051:rd2:id20:mnopqrstuvwxyz123456e2:roi1e1:t1:x1:v4:LT\x01\x021:y1:re").unwrap();
        assert_eq!(m.ip, Some(sa("1.2.3.4:5")));
        assert!(m.read_only);
        assert_eq!(m.version.as_deref(), Some(&b"LT\x01\x02"[..]));
        // A bad `ip` length or a non-1 `ro` is ignored.
        let m = decode(
            b"d2:ip5:\x01\x02\x03\x04\x001:rd2:id20:mnopqrstuvwxyz123456e2:roi2e1:t1:x1:y1:re",
        )
        .unwrap();
        assert_eq!(m.ip, None);
        assert!(!m.read_only);
        // BEP 42 raw-address forms decode with port 0: only the IP is used.
        let m =
            decode(b"d2:ip4:\x01\x02\x03\x041:rd2:id20:mnopqrstuvwxyz123456e2:roi1e1:t1:x1:y1:re")
                .unwrap();
        assert_eq!(m.ip, Some(sa("1.2.3.4:0")));
        let m = decode(
            b"d2:ip16:\x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x011:rd2:id20:mnopqrstuvwxyz123456e2:roi1e1:t1:x1:y1:re",
        )
        .unwrap();
        assert_eq!(m.ip, Some(sa("[2001:db8::1]:0")));
        // Unknown keys are ignored.
        assert!(
            decode(b"d1:rd2:id20:mnopqrstuvwxyz1234565:extrai1ee1:t1:x1:y1:r3:zzz3:zzze").is_ok()
        );
    }

    #[test]
    fn unusable_datagrams_are_dropped() {
        let cases: &[&[u8]] = &[
            b"",
            b"garbage",
            b"le",
            b"i1e",
            b"d1:y1:qe",                                                        // no t
            b"d1:t0:1:y1:qe",                                                   // empty t
            b"d1:t9:1234567891:y1:qe",                                          // t too long
            b"d1:ti1e1:y1:qe",                                                  // t not bytes
            b"d1:t2:aae",                                                       // no y
            b"d1:t2:aa1:y1:xe",                                                 // unknown y
            b"d1:t2:aa1:yi1ee",                                                 // y not bytes
            b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qeXX",      // trailing bytes
            b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:t2:bb1:y1:qe", // duplicate key
        ];
        for c in cases {
            assert_eq!(
                decode(c),
                Err(DecodeError::Unusable),
                "{}",
                String::from_utf8_lossy(c)
            );
        }
        // Depth beyond the KRPC limit.
        let mut deep = b"d1:t2:aa1:y1:q1:a".to_vec();
        deep.extend(std::iter::repeat_n(b'l', 20));
        deep.extend(std::iter::repeat_n(b'e', 21));
        assert_eq!(decode(&deep), Err(DecodeError::Unusable));
    }

    fn bad_query(input: &[u8]) -> (i64, &'static str) {
        match decode(input) {
            Err(DecodeError::BadQuery { tid, code, message }) => {
                assert_eq!(tid, b"aa");
                (code, message)
            }
            other => panic!("expected BadQuery, got {other:?}"),
        }
    }

    #[test]
    fn malformed_queries_get_errors() {
        let protocol = |input: &[u8]| assert_eq!(bad_query(input).0, error_code::PROTOCOL);
        protocol(b"d1:ad2:id20:abcdefghij0123456789e1:t2:aa1:y1:qe"); // no q
        protocol(b"d1:q4:ping1:t2:aa1:y1:qe"); // no a
        protocol(b"d1:ad2:id19:abcdefghij012345678e1:q4:ping1:t2:aa1:y1:qe"); // short id
        protocol(b"d1:ad2:idi5ee1:q4:ping1:t2:aa1:y1:qe"); // id not bytes
        protocol(b"d1:ad2:id20:abcdefghij0123456789e1:q9:find_node1:t2:aa1:y1:qe"); // no target
        protocol(b"d1:ad2:id20:abcdefghij01234567896:target3:abce1:q9:find_node1:t2:aa1:y1:qe");
        protocol(b"d1:ad2:id20:abcdefghij0123456789e1:q9:get_peers1:t2:aa1:y1:qe"); // no info_hash
        protocol(b"d1:ad2:id20:abcdefghij01234567894:wanti1ee1:q4:ping1:t2:aa1:y1:qe"); // want not a list
        protocol(b"d1:ad2:id20:abcdefghij01234567894:wantli1eee1:q4:ping1:t2:aa1:y1:qe"); // want item not bytes
        protocol(b"d1:ad2:id20:abcdefghij0123456789e1:q17:sample_infohashes1:t2:aa1:y1:qe");
        let ann = |extra: &str| {
            let mut v =
                b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz123456".to_vec();
            v.extend_from_slice(extra.as_bytes());
            v.extend_from_slice(b"e1:q13:announce_peer1:t2:aa1:y1:qe");
            v
        };
        protocol(&ann("5:token2:xx")); // no port
        protocol(&ann("4:porti0e5:token2:xx")); // port 0
        protocol(&ann("4:porti65536e5:token2:xx")); // port too large
        protocol(&ann("4:porti-1e5:token2:xx")); // negative port
        protocol(&ann("4:porti80e")); // no token
        protocol(&ann(&format!("4:porti80e5:token65:{}", "x".repeat(65)))); // token too long
        // implied_port = 1 makes the port irrelevant.
        let ok = decode(&ann("12:implied_porti1e4:porti0e5:token2:xx")).unwrap();
        let Body::Query(Query {
            method:
                Method::AnnouncePeer {
                    implied_port,
                    token,
                    ..
                },
            ..
        }) = ok.body
        else {
            panic!("not an announce")
        };
        assert!(implied_port);
        assert_eq!(token, b"xx");
        assert!(decode(&ann("12:implied_porti1e5:token2:xx")).is_ok());
        assert!(decode(&ann(&format!("4:porti80e5:token64:{}", "x".repeat(64)))).is_ok());
    }

    #[test]
    fn unknown_methods() {
        let (code, _) = bad_query(b"d1:ad2:id20:abcdefghij0123456789e1:q4:vote1:t2:aa1:y1:qe");
        assert_eq!(code, error_code::METHOD_UNKNOWN);
        // A wrong-length target is not usable either.
        let (code, _) =
            bad_query(b"d1:ad2:id20:abcdefghij01234567896:target2:xxe1:q4:vote1:t2:aa1:y1:qe");
        assert_eq!(code, error_code::METHOD_UNKNOWN);
        let m = decode(b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz123456e1:q3:put1:t2:aa1:y1:qe").unwrap();
        let Body::Query(q) = m.body else {
            panic!("not a query")
        };
        assert_eq!(
            q.method,
            Method::Other {
                name: b"put".to_vec(),
                target: NodeId(*b"mnopqrstuvwxyz123456")
            }
        );
        // A known method is never downgraded to Other.
        let m = decode(b"d1:ad2:id20:abcdefghij01234567896:target20:mnopqrstuvwxyz123456e1:q9:find_node1:t2:aa1:y1:qe").unwrap();
        assert!(matches!(
            m.body,
            Body::Query(Query {
                method: Method::FindNode { .. },
                ..
            })
        ));
    }

    #[test]
    fn want_parsing() {
        let m =
            decode(b"d1:ad2:id20:abcdefghij01234567894:wantl2:n62:xx2:n4ee1:q4:ping1:t2:aa1:y1:qe")
                .unwrap();
        let Body::Query(q) = m.body else {
            panic!("not a query")
        };
        assert_eq!(q.want, Some(Want { n4: true, n6: true }));
        let m =
            decode(b"d1:ad2:id20:abcdefghij01234567894:wantlee1:q4:ping1:t2:aa1:y1:qe").unwrap();
        let Body::Query(q) = m.body else {
            panic!("not a query")
        };
        assert_eq!(q.want, Some(Want::default()));
    }

    #[test]
    fn malformed_replies() {
        let bad: &[&[u8]] = &[
            b"d1:t2:aa1:y1:re",                 // no r
            b"d1:ri1e1:t2:aa1:y1:re",           // r not a dict
            b"d1:rd2:id3:abce1:t2:aa1:y1:re",   // short id
            b"d1:t2:aa1:y1:ee",                 // no e
            b"d1:eli201ee1:t2:aa1:y1:e1:zi0ee", // fine code, trailing key is ignored
            b"d1:ele1:t2:aa1:y1:ee",            // empty list
            b"d1:el3:abce1:t2:aa1:y1:ee",       // code not an int
        ];
        for (i, input) in bad.iter().enumerate() {
            let result = decode(input);
            if i == 4 {
                assert!(result.is_ok());
            } else {
                assert_eq!(
                    result,
                    Err(DecodeError::BadReply {
                        tid: b"aa".to_vec()
                    }),
                    "case {i}"
                );
            }
        }
    }

    #[test]
    fn response_lists_with_bad_lengths_are_discarded() {
        let mut input =
            b"d1:rd2:id20:mnopqrstuvwxyz1234565:nodes27:abcdefghij0123456789abcdefg6:nodes60:"
                .to_vec();
        input.extend_from_slice(b"7:samples21:abcdefghij0123456789x");
        input.extend_from_slice(format!("5:token65:{}", "x".repeat(65)).as_bytes());
        input.extend_from_slice(b"6:valuesl6:\x01\x02\x03\x04\x00\x055:shorti5eee1:t2:aa1:y1:re");
        let m = decode(&input).unwrap();
        let Body::Response(r) = m.body else {
            panic!("not a response")
        };
        assert_eq!(r.nodes, None);
        assert_eq!(r.nodes6, Some(vec![]));
        assert_eq!(r.samples, None);
        assert_eq!(r.token, None);
        assert_eq!(r.values, Some(vec![sa("1.2.3.4:5")]));
    }

    #[test]
    fn mainline_values_format() {
        let m = decode(b"d1:rd2:id20:mnopqrstuvwxyz1234566:valuesl12:\x01\x02\x03\x04\x00\x05\x06\x07\x08\x09\x00\x0aee1:t2:aa1:y1:re").unwrap();
        let Body::Response(r) = m.body else {
            panic!("not a response")
        };
        assert_eq!(r.values, Some(vec![sa("1.2.3.4:5"), sa("6.7.8.9:10")]));
        let m = decode(b"d1:rd2:id20:mnopqrstuvwxyz1234566:valuesl7:\x01\x02\x03\x04\x00\x05\x06ee1:t2:aa1:y1:re").unwrap();
        let Body::Response(r) = m.body else {
            panic!("not a response")
        };
        assert_eq!(r.values, Some(vec![]));
    }

    #[test]
    fn oversized_responses_are_trimmed() {
        let nodes: Vec<CompactNode> = (0..8u8)
            .map(|i| CompactNode {
                id: NodeId([i; 20]),
                addr: sa(&format!("9.9.9.{i}:99")),
            })
            .collect();
        let nodes6: Vec<CompactNode> = (0..8u8)
            .map(|i| CompactNode {
                id: NodeId([i; 20]),
                addr: sa(&format!("[2a00::{i}]:99")),
            })
            .collect();
        let values: Vec<SocketAddr> = (0..100u16)
            .map(|i| sa(&format!("[2a00::{i}]:{}", i + 1)))
            .collect();
        let r = Response {
            id: NodeId([7; 20]),
            nodes: Some(nodes.clone()),
            nodes6: Some(nodes6.clone()),
            token: Some(vec![0; 8]),
            values: Some(values.clone()),
            ..Response::default()
        };
        let m = Message {
            tid: vec![0; 8],
            ..msg(Body::Response(r))
        };
        let bytes = encode(&m).unwrap();
        assert!(bytes.len() <= MAX_DATAGRAM_OUT);
        let Body::Response(back) = decode(&bytes).unwrap().body else {
            panic!("not a response")
        };
        // Values are trimmed first; nodes survive intact.
        assert_eq!(back.nodes, Some(nodes));
        assert_eq!(back.nodes6, Some(nodes6));
        let kept = back.values.unwrap();
        assert!(!kept.is_empty() && kept.len() < values.len());
        assert_eq!(kept[..], values[..kept.len()]);
        // Close to the limit: trimming does not throw away much more than needed.
        assert!(bytes.len() > MAX_DATAGRAM_OUT - 21);

        // Samples are trimmed next.
        let samples: Vec<DhtKey> = (0..60u8).map(|i| DhtKey([i; 20])).collect();
        let r = Response {
            id: NodeId([7; 20]),
            samples: Some(samples),
            nodes: Some(vec![]),
            num: Some(60),
            interval: Some(0),
            ..Response::default()
        };
        let bytes = encode(&msg(Body::Response(r))).unwrap();
        assert!(bytes.len() <= MAX_DATAGRAM_OUT);
        let Body::Response(back) = decode(&bytes).unwrap().body else {
            panic!("not a response")
        };
        assert!(back.samples.unwrap().len() < 60);
    }

    #[test]
    fn oversized_non_responses_fail() {
        let m = msg(Body::Error(KrpcError {
            code: 201,
            message: "x".repeat(2000),
        }));
        assert_eq!(encode(&m), Err(EncodeError::TooLarge));
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..1500)) {
            let _ = decode(&bytes);
        }

        #[test]
        fn decode_of_krpc_shaped_input_never_panics(
            prefix in proptest::sample::select(vec![
                &b"d1:ad2:id20:abcdefghij0123456789"[..],
                &b"d1:rd2:id20:abcdefghij0123456789"[..],
                &b"d1:eli201e"[..],
            ]),
            middle in proptest::collection::vec(any::<u8>(), 0..300),
            suffix in proptest::sample::select(vec![&b"e1:q4:ping1:t2:aa1:y1:qe"[..], &b"e1:t2:aa1:y1:re"[..], &b"e1:t2:aa1:y1:ee"[..]]),
        ) {
            let mut input = prefix.to_vec();
            input.extend_from_slice(&middle);
            input.extend_from_slice(suffix);
            if let Ok(m) = decode(&input) {
                // Anything we accept can be re-encoded within the size cap or rejected cleanly.
                if let Ok(bytes) = encode(&m) {
                    prop_assert!(bytes.len() <= MAX_DATAGRAM_OUT);
                }
            }
        }
    }
}
