#![no_main]
//! Any datagram must decode to a message or an error, never a panic. Every
//! decoded message has bounded fields, so it must re-encode within
//! `MAX_DATAGRAM_OUT` bytes and decode again to the same message (responses
//! may lose trailing list entries to trimming; error text may be re-truncated).

use dc4_dht::krpc::{Body, MAX_DATAGRAM_OUT, MAX_ERROR_TEXT_LEN, Message, decode, encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(msg) = decode(data) else { return };
    let bytes = match encode(&msg) {
        Ok(bytes) => bytes,
        Err(e) => panic!("decoded message does not re-encode: {e}"),
    };
    assert!(bytes.len() <= MAX_DATAGRAM_OUT);
    let again = match decode(&bytes) {
        Ok(m) => m,
        Err(e) => panic!("re-encoded message does not decode: {e}"),
    };
    check_round_trip(&msg, &again);
});

fn check_round_trip(msg: &Message, again: &Message) {
    assert_eq!(again.tid, msg.tid);
    assert_eq!(again.version, msg.version);
    assert_eq!(again.ip, msg.ip);
    assert_eq!(again.read_only, msg.read_only);
    match (&msg.body, &again.body) {
        (Body::Query(q), Body::Query(q2)) => {
            assert_eq!(q, q2);
            let _ = (q.method.name(), q.method.target());
        }
        (Body::Response(r), Body::Response(r2)) => {
            assert_eq!(r2.id, r.id);
            assert_eq!(r2.token, r.token);
            assert_eq!(r2.num, r.num);
            assert_eq!(r2.interval, r.interval);
            assert_prefix(&r.nodes, &r2.nodes);
            assert_prefix(&r.nodes6, &r2.nodes6);
            assert_prefix(&r.values, &r2.values);
            assert_prefix(&r.samples, &r2.samples);
        }
        (Body::Error(e), Body::Error(e2)) => {
            assert_eq!(e2.code, e.code);
            if e.message.len() <= MAX_ERROR_TEXT_LEN {
                assert_eq!(e2.message, e.message);
            }
        }
        (a, b) => panic!("body kind changed: {a:?} -> {b:?}"),
    }
}

fn assert_prefix<T: PartialEq + std::fmt::Debug>(orig: &Option<Vec<T>>, again: &Option<Vec<T>>) {
    match (orig, again) {
        (None, None) => {}
        (Some(o), Some(a)) => assert!(o.starts_with(a), "{a:?} is not a prefix of {o:?}"),
        _ => panic!("list presence changed: {orig:?} -> {again:?}"),
    }
}
