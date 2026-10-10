#![no_main]
//! The peer-wire parsers must never panic on anything a peer can send.

use dc4_peer::fuzzing;
use dc4_peer::{MAX_FRAME_AFTER_EXT_HANDSHAKE, MAX_FRAME_BEFORE_EXT_HANDSHAKE};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = fuzzing::handshake(data);
    if let Some(prefix) = data.first_chunk::<68>() {
        let _ = fuzzing::handshake(prefix);
    }
    if let Some(header) = data.first_chunk::<4>() {
        for max in [
            0,
            MAX_FRAME_BEFORE_EXT_HANDSHAKE,
            MAX_FRAME_AFTER_EXT_HANDSHAKE,
        ] {
            if let Some(len) = fuzzing::frame_len(*header, max) {
                assert!(len <= max);
                assert_eq!(len as u64, u64::from(u32::from_be_bytes(*header)));
            }
        }
    }
    let split = fuzzing::frames(data, MAX_FRAME_AFTER_EXT_HANDSHAKE);
    let read = fuzzing::read_frames(data);
    // The async reader discards large non-extended frames that the
    // splitter refuses, so it sees at least as many frames.
    assert!(
        read >= split,
        "read_frame saw {read} frames, split_frame {split}"
    );
    fuzzing::message(data);
    if let Some((id, size)) = fuzzing::ext_handshake(data) {
        assert!(id != 0 && size > 0 && size <= dc4_peer::DEFAULT_MAX_METADATA);
    }
    let _ = fuzzing::metadata_message(data);
});
