//! Peer-wire encoding and parsing (BEP 3 handshake and framing, BEP 10
//! extension messages, BEP 9 `ut_metadata` messages).
//!
//! Parsers here are pure functions over byte slices so they can be property
//! tested; the only async function is [`read_frame`].

use dc3_bencode::{Limits, OwnedValue, Value};
use dc3_core::DhtKey;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{FetchError, MAX_DISCARD_FRAME};

/// Length of the BitTorrent handshake.
pub(crate) const HANDSHAKE_LEN: usize = 68;
/// Protocol string in the handshake.
pub(crate) const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";
/// Length of the peer ID.
pub(crate) const PEER_ID_LEN: usize = 20;
/// Offset of the reserved byte carrying the extension-protocol bit.
const RESERVED_LTEP_BYTE: usize = 5;
/// Extension-protocol bit (BEP 10).
const RESERVED_LTEP_BIT: u8 = 0x10;
/// Offset of the reserved byte carrying the DHT bit.
const RESERVED_DHT_BYTE: usize = 7;
/// DHT bit (BEP 5).
const RESERVED_DHT_BIT: u8 = 0x01;
/// Peer-wire message ID for extension messages (BEP 10).
pub(crate) const MSG_EXTENDED: u8 = 20;
/// Extended message ID of the extended handshake.
pub(crate) const EXT_HANDSHAKE_ID: u8 = 0;
/// Length of the frame length prefix.
const FRAME_HEADER_LEN: usize = 4;
/// Scratch buffer size for discarding non-extended messages.
const DISCARD_CHUNK_LEN: usize = 4 * 1024;
/// Bencode limits for extension-message dictionaries.
const EXT_LIMITS: Limits = Limits::PEER_MESSAGE;

/// `ut_metadata` message types (BEP 9).
pub(crate) const UT_REQUEST: i64 = 0;
pub(crate) const UT_DATA: i64 = 1;
pub(crate) const UT_REJECT: i64 = 2;

/// A parsed 68-byte handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handshake {
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    #[allow(dead_code)] // kept for completeness and tests
    pub peer_id: [u8; PEER_ID_LEN],
}

impl Handshake {
    pub(crate) fn supports_extensions(&self) -> bool {
        self.reserved
            .get(RESERVED_LTEP_BYTE)
            .is_some_and(|b| b & RESERVED_LTEP_BIT != 0)
    }
}

/// Reserved bytes with the extension-protocol and DHT bits set.
pub(crate) fn our_reserved() -> [u8; 8] {
    let mut r = [0u8; 8];
    if let Some(b) = r.get_mut(RESERVED_LTEP_BYTE) {
        *b |= RESERVED_LTEP_BIT;
    }
    if let Some(b) = r.get_mut(RESERVED_DHT_BYTE) {
        *b |= RESERVED_DHT_BIT;
    }
    r
}

/// Encodes a handshake.
pub(crate) fn handshake_bytes(
    reserved: [u8; 8],
    info_hash: &DhtKey,
    peer_id: &[u8; PEER_ID_LEN],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HANDSHAKE_LEN);
    // PROTOCOL is 19 bytes, so its length fits in a u8.
    out.push(PROTOCOL.len() as u8);
    out.extend_from_slice(PROTOCOL);
    out.extend_from_slice(&reserved);
    out.extend_from_slice(&info_hash.0);
    out.extend_from_slice(peer_id);
    out
}

/// Parses a handshake. `buf` must be exactly [`HANDSHAKE_LEN`] bytes.
pub(crate) fn parse_handshake(buf: &[u8]) -> Result<Handshake, FetchError> {
    if buf.len() != HANDSHAKE_LEN {
        return Err(FetchError::BadHandshake);
    }
    let (pstrlen, rest) = buf.split_first().ok_or(FetchError::BadHandshake)?;
    if usize::from(*pstrlen) != PROTOCOL.len() {
        return Err(FetchError::BadHandshake);
    }
    let (pstr, rest) = rest
        .split_at_checked(PROTOCOL.len())
        .ok_or(FetchError::BadHandshake)?;
    if pstr != PROTOCOL {
        return Err(FetchError::BadHandshake);
    }
    let (reserved, rest) = rest
        .split_first_chunk::<8>()
        .ok_or(FetchError::BadHandshake)?;
    let (info_hash, rest) = rest
        .split_first_chunk::<20>()
        .ok_or(FetchError::BadHandshake)?;
    let peer_id: &[u8; PEER_ID_LEN] = rest.try_into().map_err(|_| FetchError::BadHandshake)?;
    Ok(Handshake {
        reserved: *reserved,
        info_hash: *info_hash,
        peer_id: *peer_id,
    })
}

/// Checks a frame's length prefix against `max` before anything is allocated.
pub(crate) fn check_frame_len(
    header: [u8; FRAME_HEADER_LEN],
    max: usize,
) -> Result<usize, FetchError> {
    let raw = u32::from_be_bytes(header);
    // A u32 fits in usize on every supported (32/64-bit) target; saturate otherwise.
    let len = usize::try_from(raw).unwrap_or(usize::MAX);
    if len > max {
        return Err(FetchError::MessageTooLarge { len, max });
    }
    Ok(len)
}

/// One frame as returned by [`read_frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    /// A zero-length frame.
    KeepAlive,
    /// A non-extended message; its body was read and thrown away.
    Discarded { id: u8 },
    /// An extension message. The payload includes the leading message ID, so
    /// [`parse_message`] can classify it.
    Extended(Vec<u8>),
}

/// Reads one length-prefixed frame. The caller applies the deadline.
///
/// Extension messages (ID 20) are buffered and must be at most `ext_max`
/// bytes. Every other message is read in small chunks and discarded, and must
/// be at most [`MAX_DISCARD_FRAME`] bytes. Lengths are checked before anything
/// is read or allocated.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    ext_max: usize,
) -> Result<Frame, FetchError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    r.read_exact(&mut header).await?;
    // No message of any kind may exceed the larger of the two caps; fail at
    // once rather than wait for an ID byte that may never come.
    let len = check_frame_len(header, MAX_DISCARD_FRAME.max(ext_max))?;
    if len == 0 {
        return Ok(Frame::KeepAlive);
    }
    let id = r.read_u8().await?;
    // len >= 1 here, so this cannot underflow.
    let rest = len.saturating_sub(1);
    if id == MSG_EXTENDED {
        let len = check_frame_len(header, ext_max)?;
        let mut payload = vec![0u8; len];
        let (first, body) = payload
            .split_first_mut()
            .ok_or_else(|| FetchError::Protocol("empty extended frame".into()))?;
        *first = id;
        r.read_exact(body).await?;
        return Ok(Frame::Extended(payload));
    }
    check_frame_len(header, MAX_DISCARD_FRAME)?;
    discard(r, rest).await?;
    Ok(Frame::Discarded { id })
}

/// Reads and drops exactly `n` bytes through a fixed scratch buffer.
async fn discard<R: AsyncRead + Unpin>(r: &mut R, mut n: usize) -> Result<(), FetchError> {
    let mut scratch = [0u8; DISCARD_CHUNK_LEN];
    while n > 0 {
        let chunk = scratch
            .get_mut(..n.min(DISCARD_CHUNK_LEN))
            .unwrap_or_default();
        let got = r.read(chunk).await?;
        if got == 0 {
            return Err(FetchError::Io(std::io::ErrorKind::UnexpectedEof));
        }
        n = n.saturating_sub(got);
    }
    Ok(())
}

/// Splits one frame off the front of `buf`. Returns `None` if `buf` does not
/// yet hold a whole frame, otherwise the payload and the bytes consumed.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn split_frame(buf: &[u8], max: usize) -> Result<Option<(&[u8], usize)>, FetchError> {
    let Some((header, rest)) = buf.split_first_chunk::<FRAME_HEADER_LEN>() else {
        return Ok(None);
    };
    let len = check_frame_len(*header, max)?;
    match rest.get(..len) {
        None => Ok(None),
        // len <= max <= buf.len(), so the addition cannot overflow.
        Some(payload) => Ok(Some((payload, len.saturating_add(FRAME_HEADER_LEN)))),
    }
}

/// A peer-wire message, as far as metadata fetching cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message<'a> {
    KeepAlive,
    /// An extension message: extended ID and body.
    Extended {
        id: u8,
        body: &'a [u8],
    },
    /// Any other message, identified by its ID; ignored.
    Other(u8),
}

/// Classifies a frame payload.
pub(crate) fn parse_message(payload: &[u8]) -> Result<Message<'_>, FetchError> {
    let Some((&id, rest)) = payload.split_first() else {
        return Ok(Message::KeepAlive);
    };
    if id != MSG_EXTENDED {
        return Ok(Message::Other(id));
    }
    let (&ext_id, body) = rest
        .split_first()
        .ok_or_else(|| FetchError::Protocol("extended message without an extended id".into()))?;
    Ok(Message::Extended { id: ext_id, body })
}

/// Encodes a whole extension-message frame (length prefix included).
pub(crate) fn extended_frame(ext_id: u8, body: &[u8]) -> Result<Vec<u8>, FetchError> {
    // Two header bytes: the message ID and the extended ID.
    let len = body
        .len()
        .checked_add(2)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| FetchError::Protocol("outgoing message too large".into()))?;
    let mut out = Vec::with_capacity(body.len().saturating_add(FRAME_HEADER_LEN + 2));
    out.extend_from_slice(&len.to_be_bytes());
    out.push(MSG_EXTENDED);
    out.push(ext_id);
    out.extend_from_slice(body);
    Ok(out)
}

/// The fields of an extended handshake we care about, unvalidated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExtHandshake {
    /// `m.ut_metadata`, if present as an integer.
    pub ut_metadata: Option<i64>,
    /// `metadata_size`, if present as an integer.
    pub metadata_size: Option<i64>,
}

impl ExtHandshake {
    /// Validates the fields for a fetch: returns the peer's `ut_metadata` ID
    /// and the metadata size.
    pub(crate) fn validate(&self, max_metadata: usize) -> Result<(u8, usize), FetchError> {
        let id = match self.ut_metadata {
            None | Some(0) => return Err(FetchError::NoMetadataSupport),
            Some(id) => u8::try_from(id)
                .map_err(|_| FetchError::Protocol(format!("ut_metadata id {id} out of range")))?,
        };
        let raw = self
            .metadata_size
            .ok_or(FetchError::MetadataSizeInvalid(0))?;
        let size = usize::try_from(raw).map_err(|_| FetchError::MetadataSizeInvalid(raw))?;
        if size == 0 || size > max_metadata {
            return Err(FetchError::MetadataSizeInvalid(raw));
        }
        Ok((id, size))
    }
}

fn decode_dict_prefix(body: &[u8]) -> Result<(dc3_bencode::Dict<'_>, usize), FetchError> {
    let (value, used) = dc3_bencode::decode_prefix(body, &EXT_LIMITS)
        .map_err(|e| FetchError::Protocol(format!("bad bencode in extension message: {e}")))?;
    match value {
        Value::Dict(d) => Ok((d, used)),
        _ => Err(FetchError::Protocol(
            "extension message is not a dictionary".into(),
        )),
    }
}

/// Parses an extended-handshake body: exactly one dictionary.
pub(crate) fn parse_ext_handshake(body: &[u8]) -> Result<ExtHandshake, FetchError> {
    let (dict, used) = decode_dict_prefix(body)?;
    if used != body.len() {
        return Err(FetchError::Protocol(
            "trailing bytes in extended handshake".into(),
        ));
    }
    let ut_metadata = dict.get_dict(b"m").and_then(|m| m.get_int(b"ut_metadata"));
    Ok(ExtHandshake {
        ut_metadata,
        metadata_size: dict.get_int(b"metadata_size"),
    })
}

/// Encodes an extended-handshake body.
pub(crate) fn ext_handshake_body(
    ut_metadata: u8,
    metadata_size: Option<i64>,
    version: Option<&str>,
    reqq: Option<i64>,
) -> Vec<u8> {
    let mut m = OwnedValue::dict();
    m.insert(
        b"ut_metadata".to_vec(),
        OwnedValue::Int(i64::from(ut_metadata)),
    );
    ext_handshake_body_with_m(m, metadata_size, version, reqq)
}

/// Encodes an extended-handshake body with an arbitrary `m` dictionary.
pub(crate) fn ext_handshake_body_with_m(
    m: std::collections::BTreeMap<Vec<u8>, OwnedValue>,
    metadata_size: Option<i64>,
    version: Option<&str>,
    reqq: Option<i64>,
) -> Vec<u8> {
    let mut d = OwnedValue::dict();
    d.insert(b"m".to_vec(), OwnedValue::Dict(m));
    if let Some(size) = metadata_size {
        d.insert(b"metadata_size".to_vec(), OwnedValue::Int(size));
    }
    if let Some(v) = version {
        d.insert(b"v".to_vec(), OwnedValue::from(v));
    }
    if let Some(q) = reqq {
        d.insert(b"reqq".to_vec(), OwnedValue::Int(q));
    }
    dc3_bencode::encode(&OwnedValue::Dict(d))
}

/// A `ut_metadata` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetadataMessage<'a> {
    Request {
        piece: i64,
    },
    Data {
        piece: i64,
        total_size: i64,
        payload: &'a [u8],
    },
    Reject {
        piece: i64,
    },
    /// An unknown `msg_type`; ignored.
    Unknown(i64),
}

/// Parses a `ut_metadata` message body: exactly one dictionary for
/// request and reject messages, a dictionary followed by raw piece bytes
/// for data messages. Unknown `msg_type` values are ignored whatever
/// shape they arrive in, per BEP 9.
pub(crate) fn parse_metadata_message(body: &[u8]) -> Result<MetadataMessage<'_>, FetchError> {
    let (dict, used) = decode_dict_prefix(body)?;
    let msg_type = dict
        .get_int(b"msg_type")
        .ok_or_else(|| FetchError::Protocol("ut_metadata message without msg_type".into()))?;
    let piece = || {
        dict.get_int(b"piece")
            .ok_or_else(|| FetchError::Protocol("ut_metadata message without piece".into()))
    };
    // Only data messages carry bytes after the dictionary.
    let no_trailing = |what: &str| {
        if used != body.len() {
            return Err(FetchError::Protocol(format!("trailing bytes in {what}")));
        }
        Ok(())
    };
    match msg_type {
        UT_REQUEST => {
            let request = MetadataMessage::Request { piece: piece()? };
            no_trailing("ut_metadata request")?;
            Ok(request)
        }
        UT_DATA => {
            let total_size = dict.get_int(b"total_size").ok_or_else(|| {
                FetchError::Protocol("ut_metadata data without total_size".into())
            })?;
            let payload = body.get(used..).unwrap_or_default();
            Ok(MetadataMessage::Data {
                piece: piece()?,
                total_size,
                payload,
            })
        }
        UT_REJECT => {
            let reject = MetadataMessage::Reject { piece: piece()? };
            no_trailing("ut_metadata reject")?;
            Ok(reject)
        }
        other => {
            // Unknown types are ignored per BEP 9, whatever shape they
            // arrive in: a future extension may omit `piece` or append
            // payload bytes, and neither must fail the fetch.
            Ok(MetadataMessage::Unknown(other))
        }
    }
}

fn metadata_dict(msg_type: i64, piece: i64, total_size: Option<i64>) -> Vec<u8> {
    let mut d = OwnedValue::dict();
    d.insert(b"msg_type".to_vec(), OwnedValue::Int(msg_type));
    d.insert(b"piece".to_vec(), OwnedValue::Int(piece));
    if let Some(t) = total_size {
        d.insert(b"total_size".to_vec(), OwnedValue::Int(t));
    }
    dc3_bencode::encode(&OwnedValue::Dict(d))
}

/// Body of a `ut_metadata` request.
pub(crate) fn metadata_request_body(piece: i64) -> Vec<u8> {
    metadata_dict(UT_REQUEST, piece, None)
}

/// Body of a `ut_metadata` reject.
pub(crate) fn metadata_reject_body(piece: i64) -> Vec<u8> {
    metadata_dict(UT_REJECT, piece, None)
}

/// Body of a `ut_metadata` data message.
pub(crate) fn metadata_data_body(piece: i64, total_size: i64, data: &[u8]) -> Vec<u8> {
    let mut out = metadata_dict(UT_DATA, piece, Some(total_size));
    out.extend_from_slice(data);
    out
}
