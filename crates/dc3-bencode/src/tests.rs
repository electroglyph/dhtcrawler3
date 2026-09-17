#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use super::*;
use proptest::prelude::*;

const L: Limits = Limits {
    max_depth: 32,
    max_items: 100_000,
    max_string_len: 1 << 20,
};

fn kind(input: &[u8]) -> ErrorKind {
    decode(input, &L).unwrap_err().kind
}

#[test]
fn scalars() {
    assert_eq!(decode(b"i0e", &L).unwrap(), Value::Int(0));
    assert_eq!(decode(b"i-42e", &L).unwrap(), Value::Int(-42));
    assert_eq!(
        decode(b"i9223372036854775807e", &L).unwrap(),
        Value::Int(i64::MAX)
    );
    assert_eq!(
        decode(b"i-9223372036854775808e", &L).unwrap(),
        Value::Int(i64::MIN)
    );
    assert_eq!(decode(b"4:spam", &L).unwrap(), Value::Bytes(b"spam"));
    assert_eq!(decode(b"0:", &L).unwrap(), Value::Bytes(b""));
}

#[test]
fn invalid_integers() {
    assert_eq!(kind(b"i03e"), ErrorKind::InvalidInteger);
    assert_eq!(kind(b"i-0e"), ErrorKind::InvalidInteger);
    assert_eq!(kind(b"ie"), ErrorKind::InvalidInteger);
    assert_eq!(kind(b"i-e"), ErrorKind::InvalidInteger);
    assert_eq!(kind(b"i+1e"), ErrorKind::InvalidInteger);
    assert_eq!(kind(b"i1.5e"), ErrorKind::InvalidInteger);
    assert_eq!(kind(b"i9223372036854775808e"), ErrorKind::IntegerOverflow);
    assert_eq!(kind(b"i-9223372036854775809e"), ErrorKind::IntegerOverflow);
    assert_eq!(
        kind(b"i99999999999999999999999e"),
        ErrorKind::IntegerOverflow
    );
    assert_eq!(kind(b"i12"), ErrorKind::UnexpectedEof);
}

#[test]
fn invalid_strings() {
    assert_eq!(kind(b"04:spam"), ErrorKind::InvalidLength);
    assert_eq!(kind(b"5:spam"), ErrorKind::UnexpectedEof);
    assert_eq!(kind(b":"), ErrorKind::UnexpectedByte(b':'));
    assert_eq!(
        kind(b"99999999999999999999999:x"),
        ErrorKind::IntegerOverflow
    );
    let small = Limits {
        max_string_len: 3,
        ..L
    };
    assert_eq!(
        decode(b"4:spam", &small).unwrap_err().kind,
        ErrorKind::StringTooLong
    );
    // A huge declared length must not allocate or panic.
    assert_eq!(kind(b"18446744073709551615:x"), ErrorKind::StringTooLong);
}

#[test]
fn containers() {
    let v = decode(b"l4:spami42ee", &L).unwrap();
    assert_eq!(v, Value::List(vec![Value::Bytes(b"spam"), Value::Int(42)]));
    let v = decode(b"d3:bar4:spam3:fooi42ee", &L).unwrap();
    let d = v.as_dict().unwrap();
    assert_eq!(d.get_bytes(b"bar"), Some(&b"spam"[..]));
    assert_eq!(d.get_int(b"foo"), Some(42));
    assert!(d.is_canonical_order());
    assert_eq!(decode(b"le", &L).unwrap(), Value::List(vec![]));
    assert!(decode(b"de", &L).unwrap().as_dict().unwrap().is_empty());
}

#[test]
fn raw_value_spans() {
    let input = b"d4:infod4:name1:ae5:otheri1ee";
    let v = decode(input, &L).unwrap();
    let d = v.as_dict().unwrap();
    assert_eq!(d.raw_value(b"info"), Some(&b"d4:name1:ae"[..]));
    assert_eq!(d.raw_value(b"other"), Some(&b"i1e"[..]));
    assert_eq!(d.raw_value(b"missing"), None);
}

#[test]
fn unsorted_keys_accepted_duplicates_rejected() {
    let v = decode(b"d1:bi1e1:ai2ee", &L).unwrap();
    let d = v.as_dict().unwrap();
    assert!(!d.is_canonical_order());
    assert_eq!(d.get_int(b"a"), Some(2));
    assert_eq!(d.get_int(b"b"), Some(1));
    assert_eq!(kind(b"d1:ai1e1:ai2ee"), ErrorKind::DuplicateKey);
    // Duplicate detected after order was already broken.
    assert_eq!(kind(b"d1:bi1e1:ai2e1:bi3ee"), ErrorKind::DuplicateKey);
}

#[test]
fn non_string_keys_rejected() {
    assert_eq!(kind(b"di1ei2ee"), ErrorKind::NonStringKey);
    assert_eq!(kind(b"dlei2ee"), ErrorKind::NonStringKey);
}

#[test]
fn structural_errors() {
    assert_eq!(kind(b""), ErrorKind::UnexpectedEof);
    assert_eq!(kind(b"e"), ErrorKind::UnexpectedByte(b'e'));
    assert_eq!(kind(b"l"), ErrorKind::UnexpectedEof);
    assert_eq!(kind(b"d1:a"), ErrorKind::UnexpectedEof);
    assert_eq!(kind(b"d1:ae"), ErrorKind::UnexpectedByte(b'e'));
    assert_eq!(kind(b"x"), ErrorKind::UnexpectedByte(b'x'));
    assert_eq!(kind(b"i1ei2e"), ErrorKind::TrailingBytes);
}

#[test]
fn prefix_decoding() {
    let (v, used) = decode_prefix(b"d8:msg_typei1e5:piecei0eeRAWBYTES", &L).unwrap();
    assert_eq!(used, 25);
    assert_eq!(v.as_dict().unwrap().get_int(b"msg_type"), Some(1));
}

#[test]
fn depth_limit_is_enforced_without_recursion() {
    let depth = 200_000;
    let mut deep = vec![b'l'; depth];
    deep.extend(std::iter::repeat_n(b'e', depth));
    assert_eq!(kind(&deep), ErrorKind::TooDeep);
    let unlimited = Limits {
        max_depth: usize::MAX,
        max_items: usize::MAX,
        max_string_len: 16,
    };
    // Even with no depth limit the iterative decoder must not overflow the stack.
    let v = decode(&deep, &unlimited).unwrap();
    drop_deep(v);
    let exact = Limits { max_depth: 3, ..L };
    assert!(decode(b"llleee", &exact).is_ok());
    assert_eq!(
        decode(b"lllleeee", &exact).unwrap_err().kind,
        ErrorKind::TooDeep
    );
}

/// Dropping a deeply nested `Value` recurses in the compiler-generated drop
/// glue, so tear it down iteratively.
fn drop_deep(v: Value<'_>) {
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            Value::List(items) => stack.extend(items),
            Value::Dict(_) | Value::Bytes(_) | Value::Int(_) => {}
        }
    }
}

#[test]
fn item_limit() {
    let lim = Limits { max_items: 3, ..L };
    assert!(decode(b"li1ei2ee", &lim).is_ok());
    assert_eq!(
        decode(b"li1ei2ei3ee", &lim).unwrap_err().kind,
        ErrorKind::TooManyItems
    );
    // Dictionary keys count too.
    assert_eq!(
        decode(b"d1:ai1e1:bi2ee", &lim).unwrap_err().kind,
        ErrorKind::TooManyItems
    );
}

#[test]
fn encoder_is_canonical() {
    let mut m = OwnedValue::dict();
    m.insert(b"z".to_vec(), OwnedValue::Int(-1));
    m.insert(
        b"a".to_vec(),
        OwnedValue::List(vec!["x".into(), OwnedValue::Int(0)]),
    );
    assert_eq!(
        encode(&OwnedValue::Dict(m)),
        b"d1:al1:xi0ee1:zi-1ee".to_vec()
    );
    assert_eq!(
        encode(&OwnedValue::Int(i64::MIN)),
        b"i-9223372036854775808e".to_vec()
    );
}

#[test]
fn bep5_example_ping() {
    let q = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
    let v = decode(q, &Limits::KRPC).unwrap();
    let d = v.as_dict().unwrap();
    assert_eq!(d.get_bytes(b"q"), Some(&b"ping"[..]));
    assert_eq!(
        d.get_dict(b"a").unwrap().get_bytes(b"id"),
        Some(&b"abcdefghij0123456789"[..])
    );
    assert_eq!(encode(&v.to_owned_value()), q.to_vec());
}

fn owned_strategy() -> impl Strategy<Value = OwnedValue> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(OwnedValue::Int),
        proptest::collection::vec(any::<u8>(), 0..24).prop_map(OwnedValue::Bytes),
    ];
    leaf.prop_recursive(6, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(OwnedValue::List),
            proptest::collection::btree_map(
                proptest::collection::vec(any::<u8>(), 0..8),
                inner,
                0..8
            )
            .prop_map(OwnedValue::Dict),
        ]
    })
}

proptest! {
    #[test]
    fn round_trip(v in owned_strategy()) {
        let bytes = encode(&v);
        let decoded = decode(&bytes, &L).unwrap();
        prop_assert!(decoded.as_dict().is_none_or(Dict::is_canonical_order));
        prop_assert_eq!(decoded.to_owned_value(), v);
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode(&bytes, &Limits::KRPC);
        let _ = decode_prefix(&bytes, &Limits::KRPC);
    }

    #[test]
    fn mutated_valid_input_never_panics(v in owned_strategy(), idx in any::<usize>(), byte in any::<u8>()) {
        let mut bytes = encode(&v);
        if !bytes.is_empty() {
            let i = idx % bytes.len();
            bytes[i] = byte;
        }
        let _ = decode(&bytes, &L);
    }

    #[test]
    fn prefix_consumes_exactly_the_value(v in owned_strategy(), tail in proptest::collection::vec(any::<u8>(), 0..16)) {
        let mut bytes = encode(&v);
        let n = bytes.len();
        bytes.extend_from_slice(&tail);
        let (_, used) = decode_prefix(&bytes, &L).unwrap();
        prop_assert_eq!(used, n);
    }
}
