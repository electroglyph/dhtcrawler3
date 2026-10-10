//! Bounded, strict bencode (BEP 3).
//!
//! Every byte dhtcrawler4 receives from the network passes through this
//! decoder first, so it is written to three rules (R7 in
//! `docs/01-first-principles.md`):
//!
//! 1. **It cannot crash.** Decoding is iterative with an explicit stack, so
//!    nesting depth can never exhaust the thread stack during decoding, and
//!    every limit in [`Limits`] is checked before memory is committed.
//!    `Value`/`OwnedValue` `Drop` and `Clone` recurse (the compiler-generated
//!    impls); keep `max_depth` small or tear values down with the iterative
//!    [`Value::drop_deep`] / [`OwnedValue::drop_deep`] helpers.
//!    [`Value::to_owned_value`] is iterative and stack-safe.
//! 2. **It is strict where strictness matters.** Leading zeros, `-0`, empty
//!    integers, integers outside `i64`, non-string keys, duplicate keys and
//!    trailing bytes are errors.
//! 3. **It keeps raw spans.** [`Dict::raw_value`] returns the exact input
//!    bytes of a value, so an infohash is computed over the bytes that were
//!    received, never over a re-encoding.
//!
//! Unsorted dictionary keys are *accepted*: some clients emit them, and because
//! hashing uses raw spans, accepting them cannot change any hash.
#![forbid(unsafe_code)]
#![warn(clippy::arithmetic_side_effects)]

mod decode;
mod encode;

pub use decode::{decode, decode_prefix};
pub use encode::{OwnedValue, encode, encode_into};

use std::collections::HashSet;

/// Resource limits for one decode call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum nesting depth of values (the top-level value is depth 1).
    pub max_depth: usize,
    /// Maximum total number of decoded items (containers, scalars, and
    /// dictionary keys).
    pub max_items: usize,
    /// Maximum length of any single byte string.
    pub max_string_len: usize,
}

impl Limits {
    /// KRPC datagrams (BEP 5): small, shallow.
    pub const KRPC: Limits = Limits {
        max_depth: 8,
        max_items: 512,
        max_string_len: 2048,
    };
    /// Bencoded dictionaries inside peer-wire extension messages (BEP 10/9).
    pub const PEER_MESSAGE: Limits = Limits {
        max_depth: 8,
        max_items: 1024,
        max_string_len: 64 * 1024,
    };
    /// Torrent info dictionaries (metadata), up to 8 MiB by default.
    pub const METADATA: Limits = Limits {
        max_depth: 64,
        max_items: 1_000_000,
        max_string_len: 8 * 1024 * 1024,
    };
}

/// What went wrong, and where.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind} at byte {pos}")]
pub struct Error {
    pub kind: ErrorKind,
    pub pos: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErrorKind {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("unexpected byte 0x{0:02x}")]
    UnexpectedByte(u8),
    #[error("invalid integer")]
    InvalidInteger,
    #[error("integer out of range")]
    IntegerOverflow,
    #[error("invalid string length")]
    InvalidLength,
    #[error("string longer than limit")]
    StringTooLong,
    #[error("nesting deeper than limit")]
    TooDeep,
    #[error("more values than limit")]
    TooManyItems,
    #[error("dictionary key is not a byte string")]
    NonStringKey,
    #[error("duplicate dictionary key")]
    DuplicateKey,
    #[error("trailing bytes after value")]
    TrailingBytes,
}

/// A decoded bencode value borrowing from the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    Bytes(&'a [u8]),
    Int(i64),
    List(Vec<Value<'a>>),
    Dict(Dict<'a>),
}

impl<'a> Value<'a> {
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// The byte string as UTF-8, if it is valid UTF-8.
    pub fn as_str(&self) -> Option<&'a str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value<'a>]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dict<'a>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Copies the value into an owned tree.
    ///
    /// Iterative: uses an explicit heap work-stack, so arbitrarily deep
    /// values cannot overflow the thread stack (unlike the previous
    /// recursive implementation).
    pub fn to_owned_value(&self) -> OwnedValue {
        enum Work<'x> {
            Visit(&'x Value<'x>),
            PushKey(Vec<u8>),
            CollectList(usize),
            CollectDict(usize),
        }
        let mut work = vec![Work::Visit(self)];
        let mut results: Vec<OwnedValue> = Vec::new();
        let mut keys: Vec<Vec<u8>> = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Work::Visit(v) => match v {
                    Value::Bytes(b) => results.push(OwnedValue::Bytes(b.to_vec())),
                    Value::Int(i) => results.push(OwnedValue::Int(*i)),
                    Value::List(items) => {
                        work.push(Work::CollectList(items.len()));
                        for child in items.iter().rev() {
                            work.push(Work::Visit(child));
                        }
                    }
                    Value::Dict(d) => {
                        work.push(Work::CollectDict(d.entries.len()));
                        for e in d.entries.iter().rev() {
                            work.push(Work::Visit(&e.value));
                            work.push(Work::PushKey(e.key.to_vec()));
                        }
                    }
                },
                Work::PushKey(k) => keys.push(k),
                Work::CollectList(n) => {
                    // `n` children were visited just above, so at least `n`
                    // results are pending; the fallback only runs if that
                    // work-stack discipline is ever broken, and collects
                    // nothing then.
                    debug_assert!(results.len() >= n);
                    let start = results.len().checked_sub(n).unwrap_or(results.len());
                    let items: Vec<OwnedValue> = results.drain(start..).collect();
                    results.push(OwnedValue::List(items));
                }
                Work::CollectDict(n) => {
                    debug_assert!(results.len() >= n);
                    debug_assert!(keys.len() >= n);
                    let vstart = results.len().checked_sub(n).unwrap_or(results.len());
                    let kstart = keys.len().checked_sub(n).unwrap_or(keys.len());
                    // Drain only this frame's entries: a bare `drain(..)`
                    // would steal outer frames' pending keys/values on
                    // nested input. Collecting straight into the map also
                    // skips the two temp vectors.
                    let map = keys.drain(kstart..).zip(results.drain(vstart..)).collect();
                    results.push(OwnedValue::Dict(map));
                }
            }
        }
        results.pop().unwrap_or(OwnedValue::List(Vec::new()))
    }

    /// Tears down a (possibly deeply nested) `Value` iteratively.
    ///
    /// The compiler-generated `Drop` for `Value::List`/`Value::Dict`
    /// recurses; dropping a value decoded with a large `max_depth` via
    /// plain `drop` can overflow the thread stack. This helper moves
    /// children onto an explicit heap stack instead.
    pub fn drop_deep(value: Value<'a>) {
        let mut stack = vec![value];
        while let Some(v) = stack.pop() {
            match v {
                Value::List(items) => stack.extend(items),
                Value::Dict(d) => stack.extend(d.entries.into_iter().map(|e| e.value)),
                Value::Bytes(_) | Value::Int(_) => {}
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry<'a> {
    key: &'a [u8],
    value: Value<'a>,
    raw: &'a [u8],
}

/// A decoded dictionary. Entries are kept in input order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dict<'a> {
    entries: Vec<Entry<'a>>,
    /// True when keys arrived in strictly ascending order, which enables binary search.
    sorted: bool,
}

impl<'a> Dict<'a> {
    fn find(&self, key: &[u8]) -> Option<&Entry<'a>> {
        if self.sorted {
            self.entries
                .binary_search_by(|e| e.key.cmp(key))
                .ok()
                .and_then(|i| self.entries.get(i))
        } else {
            self.entries.iter().find(|e| e.key == key)
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<&Value<'a>> {
        self.find(key).map(|e| &e.value)
    }

    pub fn get_bytes(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.get(key).and_then(Value::as_bytes)
    }

    pub fn get_str(&self, key: &[u8]) -> Option<&'a str> {
        self.get(key).and_then(Value::as_str)
    }

    pub fn get_int(&self, key: &[u8]) -> Option<i64> {
        self.get(key).and_then(Value::as_int)
    }

    pub fn get_list(&self, key: &[u8]) -> Option<&[Value<'a>]> {
        self.get(key).and_then(Value::as_list)
    }

    pub fn get_dict(&self, key: &[u8]) -> Option<&Dict<'a>> {
        self.get(key).and_then(Value::as_dict)
    }

    /// The exact input bytes of the value stored under `key`.
    pub fn raw_value(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.find(key).map(|e| e.raw)
    }

    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.find(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the keys were in canonical (strictly ascending) order.
    pub fn is_canonical_order(&self) -> bool {
        self.sorted
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'a [u8], &Value<'a>)> + '_ {
        self.entries.iter().map(|e| (e.key, &e.value))
    }
}

/// Builder used by the decoder; tracks ordering and duplicates efficiently.
#[derive(Debug, Default)]
struct DictBuilder<'a> {
    entries: Vec<Entry<'a>>,
    sorted: bool,
    /// Populated only once keys stop arriving in ascending order.
    seen: Option<HashSet<&'a [u8]>>,
}

impl<'a> DictBuilder<'a> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            sorted: true,
            seen: None,
        }
    }

    /// Returns false if `key` is a duplicate.
    fn check_key(&mut self, key: &'a [u8]) -> bool {
        if let Some(seen) = self.seen.as_mut() {
            return seen.insert(key);
        }
        match self.entries.last() {
            None => true,
            Some(last) if last.key < key => true,
            Some(last) if last.key == key => false,
            Some(_) => {
                // Order broken: from here on, track keys in a set.
                self.sorted = false;
                let mut set: HashSet<&'a [u8]> = self.entries.iter().map(|e| e.key).collect();
                let fresh = set.insert(key);
                self.seen = Some(set);
                fresh
            }
        }
    }

    fn push(&mut self, key: &'a [u8], value: Value<'a>, raw: &'a [u8]) {
        self.entries.push(Entry { key, value, raw });
    }

    fn finish(self) -> Dict<'a> {
        Dict {
            entries: self.entries,
            sorted: self.sorted,
        }
    }
}

#[cfg(test)]
mod tests;
