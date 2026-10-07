//! Canonical bencode encoder.

use std::collections::BTreeMap;

/// An owned bencode value. Dictionaries use a `BTreeMap`, so encoding always
/// emits keys in ascending byte order (canonical form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedValue {
    Bytes(Vec<u8>),
    Int(i64),
    List(Vec<OwnedValue>),
    Dict(BTreeMap<Vec<u8>, OwnedValue>),
}

impl OwnedValue {
    pub fn bytes(b: impl Into<Vec<u8>>) -> Self {
        OwnedValue::Bytes(b.into())
    }

    pub fn dict() -> BTreeMap<Vec<u8>, OwnedValue> {
        BTreeMap::new()
    }

    /// Tears down a (possibly deeply nested) `OwnedValue` iteratively.
    ///
    /// The compiler-generated `Drop` recurses; use this for values built
    /// with large depths instead of plain `drop`.
    pub fn drop_deep(value: OwnedValue) {
        let mut stack = vec![value];
        while let Some(v) = stack.pop() {
            match v {
                OwnedValue::List(items) => stack.extend(items),
                OwnedValue::Dict(map) => stack.extend(map.into_values()),
                OwnedValue::Bytes(_) | OwnedValue::Int(_) => {}
            }
        }
    }
}

impl From<i64> for OwnedValue {
    fn from(i: i64) -> Self {
        OwnedValue::Int(i)
    }
}

impl From<&[u8]> for OwnedValue {
    fn from(b: &[u8]) -> Self {
        OwnedValue::Bytes(b.to_vec())
    }
}

impl From<&str> for OwnedValue {
    fn from(s: &str) -> Self {
        OwnedValue::Bytes(s.as_bytes().to_vec())
    }
}

impl From<BTreeMap<Vec<u8>, OwnedValue>> for OwnedValue {
    fn from(m: BTreeMap<Vec<u8>, OwnedValue>) -> Self {
        OwnedValue::Dict(m)
    }
}

/// Encodes `v` in canonical form.
pub fn encode(v: &OwnedValue) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(v, &mut out);
    out
}

/// Appends the canonical encoding of `v` to `out`.
///
/// Uses an explicit work stack, so arbitrarily deep values cannot overflow the
/// thread stack.
pub fn encode_into(v: &OwnedValue, out: &mut Vec<u8>) {
    enum Work<'v> {
        Value(&'v OwnedValue),
        Key(&'v [u8]),
        End,
    }
    let mut work = vec![Work::Value(v)];
    while let Some(item) = work.pop() {
        match item {
            Work::End => out.push(b'e'),
            Work::Key(k) => write_bytes(k, out),
            Work::Value(OwnedValue::Bytes(b)) => write_bytes(b, out),
            Work::Value(OwnedValue::Int(i)) => {
                out.push(b'i');
                write_i64(*i, out);
                out.push(b'e');
            }
            Work::Value(OwnedValue::List(items)) => {
                out.push(b'l');
                work.push(Work::End);
                for item in items.iter().rev() {
                    work.push(Work::Value(item));
                }
            }
            Work::Value(OwnedValue::Dict(map)) => {
                out.push(b'd');
                work.push(Work::End);
                for (k, val) in map.iter().rev() {
                    work.push(Work::Value(val));
                    work.push(Work::Key(k));
                }
            }
        }
    }
}

fn write_bytes(b: &[u8], out: &mut Vec<u8>) {
    write_usize(b.len(), out);
    out.push(b':');
    out.extend_from_slice(b);
}

/// Appends `v` as decimal without allocating.
fn write_i64(v: i64, out: &mut Vec<u8>) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let neg = v < 0;
    // Use unsigned magnitude so `i64::MIN` does not overflow on negation.
    let mut mag = v.unsigned_abs() as u64;
    let mut buf = [0u8; 20];
    let mut len = 0;
    while mag > 0 {
        buf[len] = b'0' + (mag % 10) as u8;
        mag /= 10;
        len += 1;
    }
    if neg {
        out.push(b'-');
    }
    for i in (0..len).rev() {
        out.push(buf[i]);
    }
}

/// Appends `v` as decimal without allocating.
fn write_usize(mut v: usize, out: &mut Vec<u8>) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut len = 0;
    while v > 0 {
        buf[len] = b'0' + (v % 10) as u8;
        v /= 10;
        len += 1;
    }
    for i in (0..len).rev() {
        out.push(buf[i]);
    }
}
