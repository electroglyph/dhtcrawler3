#![no_main]
//! Any byte string must decode to a value or an error, never a panic, and a
//! successful decode must re-encode to the same bytes when keys were canonical.

use dc4_bencode::{Limits, decode, decode_prefix, encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for limits in [Limits::KRPC, Limits::PEER_MESSAGE, Limits::METADATA] {
        if let Ok(value) = decode(data, &limits) {
            let canonical = value.as_dict().is_none_or(|d| d.is_canonical_order());
            if canonical && !has_unsorted_nested(&value) {
                assert_eq!(encode(&value.to_owned_value()), data);
            }
        }
        let _ = decode_prefix(data, &limits);
    }
});

fn has_unsorted_nested(v: &dc4_bencode::Value<'_>) -> bool {
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            dc4_bencode::Value::Dict(d) => {
                if !d.is_canonical_order() {
                    return true;
                }
                stack.extend(d.iter().map(|(_, v)| v));
            }
            dc4_bencode::Value::List(l) => stack.extend(l.iter()),
            _ => {}
        }
    }
    false
}
