//! End-to-end fetches against the test seeder and scripted peers, on 127.0.0.1 only.
// Test helpers outside #[test] fns are not covered by clippy.toml's test exemptions.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dc3_core::DhtKey;
use dc3_peer::seeder::{self, Misbehaviour, SeederStats};
use dc3_peer::{
    BYTE_BUDGET_UNIT, FetchError, FetchLimits, MAX_DISCARD_FRAME, MAX_REJECTS_SENT, fetch_metadata,
};
use sha1::{Digest, Sha1};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const PIECE: usize = 16 * 1024;

type ErrorCheck = fn(&FetchError) -> bool;

fn test_limits() -> FetchLimits {
    FetchLimits {
        connect: Duration::from_secs(1),
        handshake: Duration::from_secs(1),
        total: Duration::from_secs(3),
        max_metadata: 8 * 1024 * 1024,
        byte_budget: None,
    }
}

/// Deterministic pseudo-random bytes (xorshift), so pieces differ from each other.
fn info_of_len(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15 ^ len as u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn v1_key(info: &[u8]) -> DhtKey {
    DhtKey::from_slice(&Sha1::digest(info)).unwrap()
}

async fn start_seeder(
    key: DhtKey,
    info: Vec<u8>,
    mb: Misbehaviour,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(seeder::serve_with(listener, key, info, mb));
    (addr, task)
}

async fn fetch_timed(addr: SocketAddr, key: DhtKey) -> Result<Vec<u8>, FetchError> {
    let limits = test_limits();
    let start = Instant::now();
    let r = fetch_metadata(addr, key, &limits).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < limits.total + Duration::from_millis(500),
        "fetch took {elapsed:?}, limit {:?}",
        limits.total
    );
    r
}

#[tokio::test]
async fn happy_path_sizes() {
    for len in [1, PIECE, PIECE + 1, 1024 * 1024 + 123] {
        let info = info_of_len(len);
        let key = v1_key(&info);
        let (addr, task) = start_seeder(key, info.clone(), Misbehaviour::None).await;
        let got = fetch_timed(addr, key)
            .await
            .unwrap_or_else(|e| panic!("len {len}: {e}"));
        assert_eq!(got, info, "len {len}");
        task.abort();
    }
}

#[tokio::test]
async fn plain_serve_works() {
    let info = info_of_len(3 * PIECE);
    let key = v1_key(&info);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(seeder::serve(listener, key, info.clone()));
    assert_eq!(fetch_timed(addr, key).await.unwrap(), info);
    // The seeder keeps serving further connections.
    assert_eq!(fetch_timed(addr, key).await.unwrap(), info);
    task.abort();
}

#[tokio::test]
async fn v2_truncated_key_verifies() {
    let info = info_of_len(40_000);
    let key = DhtKey::from_slice(&Sha256::digest(&info)[..20]).unwrap();
    let (addr, task) = start_seeder(key, info.clone(), Misbehaviour::None).await;
    assert_eq!(fetch_timed(addr, key).await.unwrap(), info);
    task.abort();
}

#[tokio::test]
async fn default_limits_match_design() {
    let d = FetchLimits::default();
    assert_eq!(d.connect, Duration::from_secs(3));
    assert_eq!(d.handshake, Duration::from_secs(4));
    assert_eq!(d.total, Duration::from_secs(20));
    assert_eq!(d.max_metadata, 8 * 1024 * 1024);
    assert!(d.byte_budget.is_none());
}

#[tokio::test]
async fn misbehaviours_fail_safely() {
    // Multi-piece metadata so short and duplicate pieces matter mid-stream.
    let info = info_of_len(2 * PIECE + 100);
    let key = v1_key(&info);
    let cases: &[(Misbehaviour, ErrorCheck)] = &[
        (Misbehaviour::WrongInfoHashInHandshake, |e| {
            *e == FetchError::WrongInfoHash
        }),
        (Misbehaviour::NoLtepBit, |e| {
            *e == FetchError::NoExtensionSupport
        }),
        (Misbehaviour::OversizedFrame, |e| {
            matches!(
                e,
                FetchError::MessageTooLarge {
                    len: 17409,
                    max: 17408
                }
            )
        }),
        (Misbehaviour::ShortPiece, |e| {
            matches!(e, FetchError::Protocol(_))
        }),
        (Misbehaviour::CorruptPiece, |e| {
            *e == FetchError::HashMismatch
        }),
        (Misbehaviour::Reject, |e| *e == FetchError::Rejected),
        (Misbehaviour::HugeMetadataSize, |e| {
            *e == FetchError::MetadataSizeInvalid(1 << 40)
        }),
        (Misbehaviour::SlowLoris, |e| *e == FetchError::Timeout),
        // DuplicatePiece is not here: a redundant copy is ignored and the
        // fetch succeeds (see `duplicate_piece_is_ignored` below).
        (Misbehaviour::NoUtMetadata, |e| {
            *e == FetchError::NoMetadataSupport
        }),
        (Misbehaviour::GiantFrame, |e| {
            *e == FetchError::MessageTooLarge {
                len: MAX_DISCARD_FRAME + 1,
                max: MAX_DISCARD_FRAME,
            }
        }),
    ];
    for (mb, expected) in cases {
        let (addr, task) = start_seeder(key, info.clone(), *mb).await;
        let err = fetch_timed(addr, key)
            .await
            .expect_err("misbehaving seeder must fail");
        assert!(expected(&err), "{mb:?} gave {err:?}");
        task.abort();
    }
}

#[tokio::test]
async fn duplicate_piece_is_ignored() {
    // The seeder answers one piece twice back-to-back (the shape our own
    // retry can self-induce on a slow peer): the redundant copy is ignored
    // and the fetch still succeeds with intact bytes.
    let info = info_of_len(2 * PIECE + 100);
    let key = v1_key(&info);
    let (addr, task) = start_seeder(key, info.clone(), Misbehaviour::DuplicatePiece).await;
    assert_eq!(fetch_timed(addr, key).await.unwrap(), info);
    task.abort();
}

#[tokio::test]
async fn short_last_piece_is_rejected() {
    let info = info_of_len(1);
    let key = v1_key(&info);
    let (addr, task) = start_seeder(key, info, Misbehaviour::ShortPiece).await;
    assert!(matches!(
        fetch_timed(addr, key).await,
        Err(FetchError::Protocol(_))
    ));
    task.abort();
}

#[tokio::test]
async fn metadata_over_limit_is_refused() {
    let info = info_of_len(PIECE);
    let key = v1_key(&info);
    let (addr, task) = start_seeder(key, info, Misbehaviour::None).await;
    let limits = FetchLimits {
        max_metadata: PIECE - 1,
        ..test_limits()
    };
    let r = fetch_metadata(addr, key, &limits).await;
    assert_eq!(r, Err(FetchError::MetadataSizeInvalid(PIECE as i64)));
    task.abort();
}

#[tokio::test]
async fn seeder_closes_on_unknown_key() {
    let info = info_of_len(10);
    let key = v1_key(&info);
    let (addr, task) = start_seeder(key, info, Misbehaviour::None).await;
    let r = fetch_timed(addr, DhtKey([1; 20])).await;
    assert!(matches!(r, Err(FetchError::Io(_))), "{r:?}");
    task.abort();
}

fn raw_frame(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = ((payload.len() + 1) as u32).to_be_bytes().to_vec();
    out.push(id);
    out.extend_from_slice(payload);
    out
}

/// A scripted peer: completes the handshake, then writes `script` and waits.
async fn scripted_peer(key: DhtKey, script: Vec<u8>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut hs = [0u8; 68];
        s.read_exact(&mut hs).await.unwrap();
        let mut reply = vec![19u8];
        reply.extend_from_slice(b"BitTorrent protocol");
        reply.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0]);
        reply.extend_from_slice(&key.0);
        reply.extend_from_slice(&[b'x'; 20]);
        s.write_all(&reply).await.unwrap();
        let _ = s.write_all(&script).await;
        let mut sink = vec![0u8; 4096];
        while matches!(s.read(&mut sink).await, Ok(n) if n > 0) {}
    });
    (addr, task)
}

#[tokio::test]
async fn oversized_frame_before_ext_handshake() {
    let key = DhtKey([5; 20]);
    let mut script = Vec::new();
    // A bitfield well over 64 KiB is skipped: only extension messages have that cap.
    script.extend_from_slice(&raw_frame(5, &vec![0xff; 200 * 1024]));
    // An extension message of exactly 64 KiB (length prefix 65536) is accepted and skipped.
    let mut filler = b"\x07".to_vec();
    filler.resize(64 * 1024 - 1, 0);
    script.extend_from_slice(&raw_frame(20, &filler));
    // An extension message with id != 0 before the handshake is ignored.
    script.extend_from_slice(&raw_frame(20, b"\x07d1:xi1ee"));
    // Then an extension message one byte over 64 KiB.
    script.extend_from_slice(&(64 * 1024 + 1u32).to_be_bytes());
    script.push(20);
    script.extend_from_slice(&[0u8; 16]);
    let (addr, task) = scripted_peer(key, script).await;
    let r = fetch_timed(addr, key).await;
    assert_eq!(
        r,
        Err(FetchError::MessageTooLarge {
            len: 64 * 1024 + 1,
            max: 64 * 1024
        })
    );
    task.abort();
}

#[tokio::test]
async fn huge_length_prefix_fails_without_waiting() {
    let key = DhtKey([6; 20]);
    let (addr, task) = scripted_peer(key, u32::MAX.to_be_bytes().to_vec()).await;
    let start = Instant::now();
    let r = fetch_timed(addr, key).await;
    assert!(
        matches!(r, Err(FetchError::MessageTooLarge { .. })),
        "{r:?}"
    );
    assert!(start.elapsed() < Duration::from_millis(900));
    task.abort();
}

#[tokio::test]
async fn silent_peer_hits_handshake_timeout() {
    let key = DhtKey([8; 20]);
    let (addr, task) = scripted_peer(key, Vec::new()).await;
    let limits = test_limits();
    let start = Instant::now();
    assert_eq!(
        fetch_metadata(addr, key, &limits).await,
        Err(FetchError::Timeout)
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed >= limits.handshake && elapsed < limits.total,
        "{elapsed:?}"
    );
    task.abort();
}

#[tokio::test]
async fn closed_port_is_a_quick_connect_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let start = Instant::now();
    let r = fetch_metadata(addr, DhtKey([0; 20]), &test_limits()).await;
    assert!(matches!(r, Err(FetchError::Connect(_))), "{r:?}");
    assert_eq!(r.unwrap_err().label(), "connect");
    assert!(start.elapsed() < Duration::from_millis(500));
}

/// A bitfield for 139 257+ pieces (> 17 KiB) must not fail the fetch.
#[tokio::test]
async fn huge_bitfield_is_skipped() {
    let info = info_of_len(2 * PIECE + 100);
    let key = v1_key(&info);
    let (addr, task) = start_seeder(key, info.clone(), Misbehaviour::HugeBitfield).await;
    assert_eq!(fetch_timed(addr, key).await.unwrap(), info);
    task.abort();
}

#[tokio::test]
async fn incoming_requests_get_bounded_rejects() {
    let info = info_of_len(2 * PIECE + 100);
    let key = v1_key(&info);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stats = Arc::new(SeederStats::new());
    let task = tokio::spawn(seeder::serve_observed(
        listener,
        key,
        info.clone(),
        Misbehaviour::RequestsMetadata,
        Arc::clone(&stats),
    ));
    const { assert!(seeder::METADATA_REQUESTS_SENT > MAX_REJECTS_SENT) };
    assert_eq!(fetch_timed(addr, key).await.unwrap(), info);

    // The fetcher has closed; wait for the seeder to read everything it sent.
    let deadline = Instant::now() + Duration::from_secs(2);
    while stats.connections_closed() == 0 {
        assert!(Instant::now() < deadline, "seeder connection did not end");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(stats.rejects_received(), MAX_REJECTS_SENT);
    task.abort();
}

/// Permits a piece of `len` bytes takes from the byte budget.
fn permits_for(len: usize) -> usize {
    len.div_ceil(BYTE_BUDGET_UNIT)
}

#[tokio::test]
async fn byte_budget_too_small_times_out() {
    let info = info_of_len(2 * PIECE + 100);
    let key = v1_key(&info);
    let needed = permits_for(PIECE) * 2 + permits_for(100);
    assert_eq!(needed, 33);
    let (addr, task) = start_seeder(key, info, Misbehaviour::None).await;
    for available in [needed - 1, 1] {
        let budget = Arc::new(Semaphore::new(available));
        let limits = FetchLimits {
            total: Duration::from_millis(1500),
            byte_budget: Some(Arc::clone(&budget)),
            ..test_limits()
        };
        let start = Instant::now();
        let r = fetch_metadata(addr, key, &limits).await;
        let elapsed = start.elapsed();
        assert_eq!(r, Err(FetchError::Timeout), "{available} permits");
        assert!(
            elapsed >= limits.total && elapsed < limits.total + Duration::from_millis(500),
            "{elapsed:?}"
        );
        assert_eq!(budget.available_permits(), available);
    }
    task.abort();
}

#[tokio::test]
async fn byte_budget_just_enough_succeeds_and_is_returned() {
    let info = info_of_len(2 * PIECE + 100);
    let key = v1_key(&info);
    let needed = permits_for(info.len());
    let (addr, task) = start_seeder(key, info.clone(), Misbehaviour::None).await;
    let budget = Arc::new(Semaphore::new(needed));
    let limits = FetchLimits {
        byte_budget: Some(Arc::clone(&budget)),
        ..test_limits()
    };
    // Twice, so a leak would make the second fetch time out.
    for _ in 0..2 {
        assert_eq!(fetch_metadata(addr, key, &limits).await.unwrap(), info);
        assert_eq!(budget.available_permits(), needed);
    }
    task.abort();
}

#[tokio::test]
async fn byte_budget_is_returned_on_failure() {
    let info = info_of_len(2 * PIECE + 100);
    let key = v1_key(&info);
    let (addr, task) = start_seeder(key, info, Misbehaviour::CorruptPiece).await;
    let budget = Arc::new(Semaphore::new(1000));
    let limits = FetchLimits {
        byte_budget: Some(Arc::clone(&budget)),
        ..test_limits()
    };
    assert_eq!(
        fetch_metadata(addr, key, &limits).await,
        Err(FetchError::HashMismatch)
    );
    assert_eq!(budget.available_permits(), 1000);
    task.abort();
}

#[tokio::test]
async fn closed_byte_budget_fails_the_fetch() {
    let info = info_of_len(100);
    let key = v1_key(&info);
    let (addr, task) = start_seeder(key, info, Misbehaviour::None).await;
    let budget = Arc::new(Semaphore::new(1000));
    budget.close();
    let limits = FetchLimits {
        byte_budget: Some(budget),
        ..test_limits()
    };
    let r = fetch_metadata(addr, key, &limits).await;
    assert_eq!(r, Err(FetchError::BudgetClosed));
    assert_eq!(r.unwrap_err().label(), "budget_closed");
    task.abort();
}
