//! A small `ut_metadata` server (BEP 3, 10, 9) for tests.
//!
//! [`serve`] answers metadata requests correctly. [`serve_with`] can instead
//! misbehave in one of the ways listed in [`Misbehaviour`], so tests can check
//! that the fetcher fails safely. [`serve_observed`] also records what the
//! seeder saw in a [`SeederStats`].

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use dc3_bencode::OwnedValue;
use dc3_core::DhtKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::wire::{
    self, EXT_HANDSHAKE_ID, Frame, HANDSHAKE_LEN, MSG_EXTENDED, Message, MetadataMessage,
    PEER_ID_LEN,
};
use crate::{
    FetchError, MAX_DISCARD_FRAME, MAX_FRAME_AFTER_EXT_HANDSHAKE, MAX_FRAME_BEFORE_EXT_HANDSHAKE,
    MAX_REJECTS_SENT, METADATA_PIECE_LEN,
};

/// Extension message ID the seeder advertises for `ut_metadata`.
pub const SEEDER_UT_METADATA_ID: u8 = 3;
/// `metadata_size` advertised under [`Misbehaviour::HugeMetadataSize`] (1 TiB).
pub const HUGE_METADATA_SIZE: i64 = 1 << 40;
/// Delay between bytes under [`Misbehaviour::SlowLoris`].
pub const SLOW_LORIS_INTERVAL: Duration = Duration::from_millis(200);
/// Size of the bitfield sent under [`Misbehaviour::HugeBitfield`] (1 MiB).
pub const HUGE_BITFIELD_LEN: usize = 1024 * 1024;
/// Length prefix sent under [`Misbehaviour::GiantFrame`]: one byte over
/// [`MAX_DISCARD_FRAME`].
pub const GIANT_FRAME_LEN: usize = MAX_DISCARD_FRAME + 1;
/// Metadata requests sent to the fetcher under
/// [`Misbehaviour::RequestsMetadata`]; more than it may answer.
pub const METADATA_REQUESTS_SENT: usize = MAX_REJECTS_SENT + 4;
/// Upper bound on the life of one seeder connection.
pub const SEEDER_CONNECTION_TIMEOUT: Duration = Duration::from_secs(120);
/// Pause after a failed `accept`, so resource exhaustion does not spin.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
/// Peer ID the seeder sends.
const SEEDER_PEER_ID: &[u8; PEER_ID_LEN] = b"-DC0100-seeder000000";
/// Peer-wire message ID of `unchoke`, sent before the extended handshake so
/// fetchers must skip a non-extended message.
const MSG_UNCHOKE: u8 = 1;
/// Peer-wire message ID of `bitfield`.
const MSG_BITFIELD: u8 = 5;
/// Bytes of body sent after the header under [`Misbehaviour::GiantFrame`].
const GIANT_FRAME_SENT_BODY: usize = 16;

/// Ways the seeder can misbehave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Misbehaviour {
    /// Serve correctly.
    #[default]
    None,
    /// Reply with a different infohash in the handshake.
    WrongInfoHashInHandshake,
    /// Omit the extension-protocol bit from the handshake.
    NoLtepBit,
    /// Answer a request with a frame longer than the post-handshake limit.
    OversizedFrame,
    /// Send each piece one byte short.
    ShortPiece,
    /// Flip a bit in each piece.
    CorruptPiece,
    /// Reject every request.
    Reject,
    /// Advertise a `metadata_size` of [`HUGE_METADATA_SIZE`].
    HugeMetadataSize,
    /// Send the extended handshake one byte every [`SLOW_LORIS_INTERVAL`].
    SlowLoris,
    /// Send every piece twice.
    DuplicatePiece,
    /// Advertise no `ut_metadata` in the extended handshake.
    NoUtMetadata,
    /// Send a [`HUGE_BITFIELD_LEN`] bitfield before the extended handshake,
    /// and another before the first piece, then serve correctly. The fetch
    /// must succeed.
    HugeBitfield,
    /// Send [`METADATA_REQUESTS_SENT`] metadata requests to the fetcher, then
    /// serve correctly. Rejects that come back are counted in
    /// [`SeederStats::rejects_received`].
    RequestsMetadata,
    /// Send a non-extended frame whose length prefix is [`GIANT_FRAME_LEN`]
    /// instead of the extended handshake.
    GiantFrame,
}

/// What a seeder observed, shared with the test that started it.
#[derive(Debug, Default)]
pub struct SeederStats {
    rejects_received: AtomicUsize,
    connections_closed: AtomicUsize,
}

impl SeederStats {
    /// A new, zeroed counter set.
    pub fn new() -> Self {
        Self::default()
    }

    /// `ut_metadata` rejects received from fetchers, over all connections.
    pub fn rejects_received(&self) -> usize {
        self.rejects_received.load(Ordering::SeqCst)
    }

    /// Connections whose handler has finished.
    pub fn connections_closed(&self) -> usize {
        self.connections_closed.load(Ordering::SeqCst)
    }
}

/// Serves `info` for `key` on `listener` until the task is dropped.
pub async fn serve(listener: TcpListener, key: DhtKey, info: Vec<u8>) {
    serve_with(listener, key, info, Misbehaviour::None).await;
}

/// Like [`serve`], but misbehaves as requested.
pub async fn serve_with(
    listener: TcpListener,
    key: DhtKey,
    info: Vec<u8>,
    misbehaviour: Misbehaviour,
) {
    serve_observed(
        listener,
        key,
        info,
        misbehaviour,
        Arc::new(SeederStats::new()),
    )
    .await;
}

/// Like [`serve_with`], and records observations in `stats`.
pub async fn serve_observed(
    listener: TcpListener,
    key: DhtKey,
    info: Vec<u8>,
    misbehaviour: Misbehaviour,
    stats: Arc<SeederStats>,
) {
    let info = Arc::new(info);
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let info = Arc::clone(&info);
                let stats = Arc::clone(&stats);
                tokio::spawn(async move {
                    let conn = handle(stream, key, info, misbehaviour, &stats);
                    match tokio::time::timeout(SEEDER_CONNECTION_TIMEOUT, conn).await {
                        Ok(Ok(())) | Err(_) => {}
                        Ok(Err(e)) => {
                            tracing::debug!(reason = e.label(), "seeder connection ended")
                        }
                    }
                    stats.connections_closed.fetch_add(1, Ordering::SeqCst);
                });
            }
            Err(e) => {
                tracing::debug!(error = %e, "seeder accept failed");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
            }
        }
    }
}

fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
    // Test payloads are far below u32::MAX; saturate rather than panic.
    let len = u32::try_from(payload.len().saturating_add(1)).unwrap_or(u32::MAX);
    let mut out = len.to_be_bytes().to_vec();
    out.push(id);
    out.extend_from_slice(payload);
    out
}

async fn handle(
    stream: TcpStream,
    key: DhtKey,
    info: Arc<Vec<u8>>,
    mb: Misbehaviour,
    stats: &SeederStats,
) -> Result<(), FetchError> {
    let _ = stream.set_nodelay(true);
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    let mut theirs = [0u8; HANDSHAKE_LEN];
    rd.read_exact(&mut theirs).await?;
    let hs = wire::parse_handshake(&theirs)?;
    if hs.info_hash != key.0 {
        return Ok(());
    }

    let reserved = if mb == Misbehaviour::NoLtepBit {
        [0u8; 8]
    } else {
        wire::our_reserved()
    };
    let mut reply_hash = key;
    if mb == Misbehaviour::WrongInfoHashInHandshake {
        for b in &mut reply_hash.0 {
            *b ^= 0xff;
        }
    }
    wr.write_all(&wire::handshake_bytes(
        reserved,
        &reply_hash,
        SEEDER_PEER_ID,
    ))
    .await?;

    let mut m = OwnedValue::dict();
    if mb != Misbehaviour::NoUtMetadata {
        m.insert(
            b"ut_metadata".to_vec(),
            OwnedValue::Int(i64::from(SEEDER_UT_METADATA_ID)),
        );
    }
    let size = if mb == Misbehaviour::HugeMetadataSize {
        HUGE_METADATA_SIZE
    } else {
        i64::try_from(info.len()).unwrap_or(i64::MAX)
    };
    let body = wire::ext_handshake_body_with_m(m, Some(size), Some("dc3-seeder"), None);
    let ext = wire::extended_frame(EXT_HANDSHAKE_ID, &body)?;

    // A keep-alive and an unchoke first: the fetcher must skip them.
    let mut preamble = 0u32.to_be_bytes().to_vec();
    preamble.extend_from_slice(&frame(MSG_UNCHOKE, &[]));

    if mb == Misbehaviour::HugeBitfield {
        preamble.extend_from_slice(&frame(MSG_BITFIELD, &vec![0xff; HUGE_BITFIELD_LEN]));
    }

    if mb == Misbehaviour::GiantFrame {
        let len = u32::try_from(GIANT_FRAME_LEN).unwrap_or(u32::MAX);
        preamble.extend_from_slice(&len.to_be_bytes());
        preamble.push(MSG_BITFIELD);
        preamble.resize(preamble.len().saturating_add(GIANT_FRAME_SENT_BODY), 0xff);
        wr.write_all(&preamble).await?;
        let mut sink = [0u8; 1024];
        while rd.read(&mut sink).await? > 0 {}
        return Ok(());
    }

    if mb == Misbehaviour::SlowLoris {
        wr.write_all(&preamble).await?;
        for b in ext {
            tokio::time::sleep(SLOW_LORIS_INTERVAL).await;
            wr.write_all(&[b]).await?;
        }
        // Then go silent until the fetcher gives up.
        let mut sink = [0u8; 1024];
        while rd.read(&mut sink).await? > 0 {}
        return Ok(());
    }

    preamble.extend_from_slice(&ext);
    wr.write_all(&preamble).await?;

    let mut their_ut_id: Option<u8> = None;
    let piece_count = info.len().div_ceil(METADATA_PIECE_LEN);
    let mut second_bitfield_sent = false;
    loop {
        let payload = match wire::read_frame(&mut rd, MAX_FRAME_BEFORE_EXT_HANDSHAKE).await {
            Ok(Frame::Extended(p)) => p,
            Ok(Frame::KeepAlive | Frame::Discarded { .. }) => continue,
            Err(FetchError::Io(std::io::ErrorKind::UnexpectedEof)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let (id, body) = match wire::parse_message(&payload)? {
            Message::Extended { id, body } => (id, body),
            Message::KeepAlive | Message::Other(_) => continue,
        };
        if id == EXT_HANDSHAKE_ID {
            their_ut_id = wire::parse_ext_handshake(body)?
                .ut_metadata
                .and_then(|v| u8::try_from(v).ok())
                .filter(|v| *v != 0);
            if let (Misbehaviour::RequestsMetadata, Some(out_id)) = (mb, their_ut_id) {
                let mut out = Vec::new();
                // In-range pieces, cycling: every request must be answered,
                // but answers stop at the cap.
                for piece in 0..METADATA_REQUESTS_SENT {
                    let piece = i64::try_from(piece % piece_count.max(1)).unwrap_or(i64::MAX);
                    out.extend_from_slice(&wire::extended_frame(
                        out_id,
                        &wire::metadata_request_body(piece),
                    )?);
                }
                wr.write_all(&out).await?;
            }
            continue;
        }
        if id != SEEDER_UT_METADATA_ID {
            continue;
        }
        let piece = match wire::parse_metadata_message(body)? {
            MetadataMessage::Request { piece } => piece,
            MetadataMessage::Reject { .. } => {
                stats.rejects_received.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            MetadataMessage::Data { .. } | MetadataMessage::Unknown(_) => continue,
        };
        let Some(out_id) = their_ut_id else { continue };
        if mb == Misbehaviour::HugeBitfield && !second_bitfield_sent {
            second_bitfield_sent = true;
            wr.write_all(&frame(MSG_BITFIELD, &vec![0xff; HUGE_BITFIELD_LEN]))
                .await?;
        }

        let data = usize::try_from(piece)
            .ok()
            .filter(|i| *i < piece_count)
            .and_then(|i| {
                let start = i.checked_mul(METADATA_PIECE_LEN)?;
                let end = start.checked_add(METADATA_PIECE_LEN)?.min(info.len());
                info.get(start..end)
            });
        let Some(data) = data.filter(|_| mb != Misbehaviour::Reject) else {
            wr.write_all(&wire::extended_frame(
                out_id,
                &wire::metadata_reject_body(piece),
            )?)
            .await?;
            continue;
        };

        let out = match mb {
            Misbehaviour::OversizedFrame => {
                let len = MAX_FRAME_AFTER_EXT_HANDSHAKE.saturating_add(1);
                let mut out = u32::try_from(len)
                    .unwrap_or(u32::MAX)
                    .to_be_bytes()
                    .to_vec();
                // An extension message, so the post-handshake cap applies.
                out.push(MSG_EXTENDED);
                out.resize(len.saturating_add(4), 0);
                out
            }
            Misbehaviour::ShortPiece => {
                let short = data.get(..data.len().saturating_sub(1)).unwrap_or_default();
                wire::extended_frame(out_id, &wire::metadata_data_body(piece, size, short))?
            }
            Misbehaviour::CorruptPiece => {
                let mut bad = data.to_vec();
                if let Some(b) = bad.first_mut() {
                    *b ^= 0x01;
                }
                wire::extended_frame(out_id, &wire::metadata_data_body(piece, size, &bad))?
            }
            Misbehaviour::DuplicatePiece => {
                let one =
                    wire::extended_frame(out_id, &wire::metadata_data_body(piece, size, data))?;
                let mut two = one.clone();
                two.extend_from_slice(&one);
                two
            }
            _ => wire::extended_frame(out_id, &wire::metadata_data_body(piece, size, data))?,
        };
        wr.write_all(&out).await?;
    }
}
