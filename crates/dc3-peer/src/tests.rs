use proptest::prelude::*;

use crate::assembly::Assembly;
use crate::wire::{self, ExtHandshake, Frame, Message, MetadataMessage};
use crate::{
    BYTE_BUDGET_UNIT, FetchError, MAX_DISCARD_FRAME, MAX_FRAME_AFTER_EXT_HANDSHAKE,
    MAX_FRAME_BEFORE_EXT_HANDSHAKE, MAX_REJECTS_SENT, METADATA_PIECE_LEN,
};
use dc3_core::DhtKey;

#[test]
fn handshake_round_trip() {
    let key = DhtKey([7; 20]);
    let bytes = wire::handshake_bytes(wire::our_reserved(), &key, b"-DC0100-abcdefghijkl");
    assert_eq!(bytes.len(), wire::HANDSHAKE_LEN);
    let hs = wire::parse_handshake(&bytes).unwrap();
    assert_eq!(hs.info_hash, key.0);
    assert_eq!(&hs.peer_id, b"-DC0100-abcdefghijkl");
    assert!(hs.supports_extensions());
    assert_eq!(hs.reserved[5], 0x10);
    assert_eq!(hs.reserved[7], 0x01);

    let no_ltep = wire::handshake_bytes([0; 8], &key, b"-DC0100-abcdefghijkl");
    assert!(
        !wire::parse_handshake(&no_ltep)
            .unwrap()
            .supports_extensions()
    );
}

#[test]
fn handshake_rejects_bad_input() {
    let key = DhtKey([7; 20]);
    let good = wire::handshake_bytes(wire::our_reserved(), &key, &[0; 20]);
    let mut bad = good.clone();
    bad[0] = 18;
    assert_eq!(wire::parse_handshake(&bad), Err(FetchError::BadHandshake));
    let mut bad = good.clone();
    bad[1] = b'b';
    assert_eq!(wire::parse_handshake(&bad), Err(FetchError::BadHandshake));
    assert_eq!(
        wire::parse_handshake(&good[..67]),
        Err(FetchError::BadHandshake)
    );
}

#[test]
fn frame_length_is_checked_before_reading() {
    assert_eq!(wire::check_frame_len(16u32.to_be_bytes(), 16), Ok(16));
    assert_eq!(
        wire::check_frame_len(17u32.to_be_bytes(), 16),
        Err(FetchError::MessageTooLarge { len: 17, max: 16 })
    );
    // Header claims 4 GiB but only the header is present: must fail, not wait or allocate.
    assert!(matches!(
        wire::split_frame(&u32::MAX.to_be_bytes(), MAX_FRAME_BEFORE_EXT_HANDSHAKE),
        Err(FetchError::MessageTooLarge { .. })
    ));
    assert_eq!(wire::split_frame(&[0, 0, 0, 2, 1], 16), Ok(None));
    assert_eq!(
        wire::split_frame(&[0, 0, 0, 2, 1, 2, 3], 16),
        Ok(Some((&[1u8, 2][..], 6)))
    );
}

#[tokio::test]
async fn read_frame_handles_keepalive_and_eof() {
    let mut input: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 1, 5, 0, 0, 0, 2, 20, 9, 0, 0];
    assert_eq!(
        wire::read_frame(&mut input, 16).await.unwrap(),
        Frame::KeepAlive
    );
    assert_eq!(
        wire::read_frame(&mut input, 16).await.unwrap(),
        Frame::Discarded { id: 5 }
    );
    assert_eq!(
        wire::read_frame(&mut input, 16).await.unwrap(),
        Frame::Extended(vec![20, 9])
    );
    assert!(matches!(
        wire::read_frame(&mut input, 16).await,
        Err(FetchError::Io(_))
    ));
}

#[tokio::test]
async fn read_frame_discards_large_non_extended_frames() {
    // A bitfield far over the extended cap is skipped, and the next frame is intact.
    let big = MAX_FRAME_AFTER_EXT_HANDSHAKE * 3;
    let mut bytes = u32::try_from(big).unwrap().to_be_bytes().to_vec();
    bytes.push(5);
    bytes.resize(bytes.len() + big - 1, 0xff);
    bytes.extend_from_slice(&[0, 0, 0, 3, 20, 1, 2]);
    let mut input: &[u8] = &bytes;
    assert_eq!(
        wire::read_frame(&mut input, MAX_FRAME_AFTER_EXT_HANDSHAKE)
            .await
            .unwrap(),
        Frame::Discarded { id: 5 }
    );
    assert_eq!(
        wire::read_frame(&mut input, MAX_FRAME_AFTER_EXT_HANDSHAKE)
            .await
            .unwrap(),
        Frame::Extended(vec![20, 1, 2])
    );
    assert!(input.is_empty());
}

#[tokio::test]
async fn read_frame_applies_the_right_cap() {
    // Exactly MAX_DISCARD_FRAME is accepted for a non-extended message.
    let mut bytes = u32::try_from(MAX_DISCARD_FRAME)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    bytes.push(5);
    bytes.resize(bytes.len() + MAX_DISCARD_FRAME - 1, 0);
    let mut input: &[u8] = &bytes;
    assert_eq!(
        wire::read_frame(&mut input, 16).await.unwrap(),
        Frame::Discarded { id: 5 }
    );

    // One more byte fails on the header alone.
    let over = MAX_DISCARD_FRAME + 1;
    let mut input: &[u8] = &u32::try_from(over).unwrap().to_be_bytes();
    assert_eq!(
        wire::read_frame(&mut input, 16).await,
        Err(FetchError::MessageTooLarge {
            len: over,
            max: MAX_DISCARD_FRAME
        })
    );

    // An extended frame one byte over its cap fails after the ID byte.
    let mut input: &[u8] = &[0, 0, 0, 17, 20];
    assert_eq!(
        wire::read_frame(&mut input, 16).await,
        Err(FetchError::MessageTooLarge { len: 17, max: 16 })
    );
    // A truncated body is an I/O error, not a hang or panic.
    let mut input: &[u8] = &[0, 0, 0, 9, 5, 1, 2];
    assert!(matches!(
        wire::read_frame(&mut input, 16).await,
        Err(FetchError::Io(_))
    ));
}

#[test]
fn byte_budget_constants() {
    assert_eq!(BYTE_BUDGET_UNIT, 1024);
    assert_eq!(MAX_DISCARD_FRAME, 4 * 1024 * 1024);
    assert_eq!(MAX_REJECTS_SENT, 8);
    assert_eq!(MAX_FRAME_AFTER_EXT_HANDSHAKE, 16 * 1024 + 1024);
    assert_eq!(MAX_FRAME_BEFORE_EXT_HANDSHAKE, 64 * 1024);
}

#[test]
fn assembly_validate_does_not_store() {
    let mut a = Assembly::new(METADATA_PIECE_LEN + 1).unwrap();
    let _ = a.next_requests();
    let size = i64::try_from(METADATA_PIECE_LEN + 1).unwrap();
    assert_eq!(a.validate(1, size, &[7]), Ok(1));
    assert_eq!(a.validate(1, size, &[7]), Ok(1));
    a.accept(1, size, &[7]).unwrap();
    assert!(matches!(
        a.validate(1, size, &[7]),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        a.validate(2, size, &[7]),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        a.validate(0, size, &[7]),
        Err(FetchError::Protocol(_))
    ));
    assert!(!a.is_complete());
}

#[test]
fn message_classification() {
    assert_eq!(wire::parse_message(&[]), Ok(Message::KeepAlive));
    assert_eq!(wire::parse_message(&[5, 0xff]), Ok(Message::Other(5)));
    assert_eq!(
        wire::parse_message(&[20, 0, b'd', b'e']),
        Ok(Message::Extended { id: 0, body: b"de" })
    );
    assert!(matches!(
        wire::parse_message(&[20]),
        Err(FetchError::Protocol(_))
    ));
}

#[test]
fn ext_handshake_parse_and_validate() {
    let body = wire::ext_handshake_body(3, Some(1234), Some("x"), Some(250));
    let hs = wire::parse_ext_handshake(&body).unwrap();
    assert_eq!(
        hs,
        ExtHandshake {
            ut_metadata: Some(3),
            metadata_size: Some(1234)
        }
    );
    assert_eq!(hs.validate(2000), Ok((3, 1234)));
    assert_eq!(
        hs.validate(1000),
        Err(FetchError::MetadataSizeInvalid(1234))
    );

    let v = |ut, size| {
        ExtHandshake {
            ut_metadata: ut,
            metadata_size: size,
        }
        .validate(100)
    };
    assert_eq!(v(None, Some(1)), Err(FetchError::NoMetadataSupport));
    assert_eq!(v(Some(0), Some(1)), Err(FetchError::NoMetadataSupport));
    assert!(matches!(
        v(Some(256), Some(1)),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(v(Some(-1), Some(1)), Err(FetchError::Protocol(_))));
    assert_eq!(v(Some(255), Some(100)), Ok((255, 100)));
    assert_eq!(v(Some(1), Some(0)), Err(FetchError::MetadataSizeInvalid(0)));
    assert_eq!(
        v(Some(1), Some(-5)),
        Err(FetchError::MetadataSizeInvalid(-5))
    );
    assert!(matches!(
        v(Some(1), None),
        Err(FetchError::MetadataSizeInvalid(_))
    ));

    // `m` of the wrong type means no support, not a crash.
    assert_eq!(
        wire::parse_ext_handshake(b"d1:mi5ee").unwrap(),
        ExtHandshake {
            ut_metadata: None,
            metadata_size: None
        }
    );
    assert!(matches!(
        wire::parse_ext_handshake(b"i5e"),
        Err(FetchError::Protocol(_))
    ));
}

#[test]
fn metadata_messages() {
    let data = wire::metadata_data_body(2, 40000, b"abc");
    assert_eq!(
        wire::parse_metadata_message(&data),
        Ok(MetadataMessage::Data {
            piece: 2,
            total_size: 40000,
            payload: b"abc"
        })
    );
    assert_eq!(
        wire::parse_metadata_message(&wire::metadata_request_body(4)),
        Ok(MetadataMessage::Request { piece: 4 })
    );
    assert_eq!(
        wire::parse_metadata_message(&wire::metadata_reject_body(1)),
        Ok(MetadataMessage::Reject { piece: 1 })
    );
    assert_eq!(
        wire::parse_metadata_message(b"d8:msg_typei9e5:piecei3ee"),
        Ok(MetadataMessage::Unknown(9))
    );
    // An unknown type without a piece is malformed, not ignorable.
    assert!(matches!(
        wire::parse_metadata_message(b"d8:msg_typei9ee"),
        Err(FetchError::Protocol(_))
    ));
    // Trailing bytes are rejected on non-data messages ...
    let mut trailing = wire::metadata_request_body(4);
    trailing.extend_from_slice(b"junk");
    assert!(matches!(
        wire::parse_metadata_message(&trailing),
        Err(FetchError::Protocol(_))
    ));
    let mut trailing = wire::metadata_reject_body(1);
    trailing.push(0);
    assert!(matches!(
        wire::parse_metadata_message(&trailing),
        Err(FetchError::Protocol(_))
    ));
    // ... and on the extended handshake, whose body is one dictionary.
    let mut hs = wire::ext_handshake_body(1, None, None, None);
    hs.extend_from_slice(b"de");
    assert!(matches!(
        wire::parse_ext_handshake(&hs),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        wire::parse_metadata_message(b"d8:msg_typei1e5:piecei0ee"),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        wire::parse_metadata_message(b"de"),
        Err(FetchError::Protocol(_))
    ));
}

#[test]
fn assembly_pipelines_and_orders() {
    let size = 5 * METADATA_PIECE_LEN + 10;
    let mut a = Assembly::new(size).unwrap();
    assert_eq!(a.next_requests(), vec![0, 1, 2, 3]);
    assert_eq!(a.next_requests(), Vec::<usize>::new());
    let total = size as i64;
    let full = vec![1u8; METADATA_PIECE_LEN];
    // Out of order and unrequested pieces are accepted.
    a.accept(5, total, &[9u8; 10]).unwrap();
    a.accept(2, total, &full).unwrap();
    assert_eq!(a.next_requests(), vec![4]);
    assert_eq!(a.next_requests(), Vec::<usize>::new());
    assert!(matches!(
        a.accept(2, total, &full),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        a.accept(6, total, &[0]),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        a.accept(-1, total, &full),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        a.accept(0, total + 1, &full),
        Err(FetchError::Protocol(_))
    ));
    assert!(matches!(
        a.accept(0, total, &full[1..]),
        Err(FetchError::Protocol(_))
    ));
    for i in [0, 1, 3, 4] {
        a.accept(i, total, &full).unwrap();
    }
    assert!(a.is_complete());
    let out = a.finish().unwrap();
    assert_eq!(out.len(), size);
    assert_eq!(&out[size - 10..], &[9u8; 10]);
    assert!(Assembly::new(0).is_err());
}

#[test]
fn piece_range_and_awaiting() {
    let size = 2 * METADATA_PIECE_LEN + 1;
    let mut a = Assembly::new(size).unwrap();
    // Three pieces: 0, 1, 2.
    assert!(a.has_piece(0));
    assert!(a.has_piece(2));
    assert!(!a.has_piece(3));
    assert!(!a.has_piece(-1));
    assert!(!a.has_piece(i64::MAX));
    // Nothing requested yet: no reject fails the fetch.
    assert!(!a.is_awaiting(0));
    assert_eq!(a.next_requests(), vec![0, 1, 2]);
    assert!(a.is_awaiting(0));
    assert!(a.is_awaiting(2));
    assert!(!a.is_awaiting(3));
    assert!(!a.is_awaiting(-1));
}

#[test]
fn verify_v1_and_v2() {
    use sha1::{Digest, Sha1};
    use sha2::Sha256;
    let info = b"d4:name3:abce";
    let v1 = DhtKey::from_slice(&Sha1::digest(info)).unwrap();
    let v2 = DhtKey::from_slice(&Sha256::digest(info)[..20]).unwrap();
    assert!(crate::verify_metadata(&v1, info));
    assert!(crate::verify_metadata(&v2, info));
    assert!(!crate::verify_metadata(&DhtKey([0; 20]), info));
}

#[test]
fn labels_are_distinct() {
    let all = [
        FetchError::Connect(std::io::ErrorKind::ConnectionRefused),
        FetchError::Timeout,
        FetchError::Io(std::io::ErrorKind::UnexpectedEof),
        FetchError::BadHandshake,
        FetchError::WrongInfoHash,
        FetchError::NoExtensionSupport,
        FetchError::NoMetadataSupport,
        FetchError::MetadataSizeInvalid(0),
        FetchError::MessageTooLarge { len: 1, max: 0 },
        FetchError::Protocol(String::new()),
        FetchError::Rejected,
        FetchError::HashMismatch,
        FetchError::BudgetClosed,
    ];
    let labels: std::collections::HashSet<_> = all.iter().map(FetchError::label).collect();
    assert_eq!(labels.len(), all.len());
}

proptest! {
    #[test]
    fn handshake_parser_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..140)) {
        let _ = wire::parse_handshake(&bytes);
    }

    #[test]
    fn handshake_parser_on_right_length(mut bytes in proptest::collection::vec(any::<u8>(), 68)) {
        bytes[0] = 19;
        let _ = wire::parse_handshake(&bytes);
    }

    #[test]
    fn frame_and_message_parsers_never_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        max in 0usize..MAX_FRAME_BEFORE_EXT_HANDSHAKE,
    ) {
        let mut rest: &[u8] = &bytes;
        while let Ok(Some((payload, used))) = wire::split_frame(rest, max) {
            prop_assert!(used >= 4 && used <= rest.len());
            if let Ok(Message::Extended { id, body }) = wire::parse_message(payload) {
                if id == 0 {
                    if let Ok(hs) = wire::parse_ext_handshake(body) {
                        let _ = hs.validate(MAX_FRAME_AFTER_EXT_HANDSHAKE);
                    }
                } else {
                    let _ = wire::parse_metadata_message(body);
                }
            }
            rest = &rest[used..];
        }
    }

    #[test]
    fn extension_parsers_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = wire::parse_message(&bytes);
        let _ = wire::parse_ext_handshake(&bytes);
        let _ = wire::parse_metadata_message(&bytes);
    }

    #[test]
    fn extension_parsers_on_dict_like_input(
        tail in proptest::collection::vec(any::<u8>(), 0..128),
        msg_type in any::<i64>(),
        piece in any::<i64>(),
        total in any::<i64>(),
    ) {
        let mut body = format!("d8:msg_typei{msg_type}e5:piecei{piece}e10:total_sizei{total}ee").into_bytes();
        body.extend_from_slice(&tail);
        let _ = wire::parse_metadata_message(&body);
        let mut a = Assembly::new(3 * METADATA_PIECE_LEN + 1).unwrap();
        let _ = a.next_requests();
        let _ = a.accept(piece, total, &tail);
    }

    #[test]
    fn async_frame_reader_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let mut input: &[u8] = &bytes;
            while let Ok(frame) = wire::read_frame(&mut input, MAX_FRAME_AFTER_EXT_HANDSHAKE).await {
                if let Frame::Extended(payload) = frame {
                    prop_assert_eq!(payload.first(), Some(&20));
                    let _ = wire::parse_message(&payload);
                }
            }
            Ok(())
        })?;
    }
}
