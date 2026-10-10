//! Iterative bencode decoder.

use crate::{DictBuilder, Error, ErrorKind, Limits, Value};

/// Decodes exactly one value that must span the whole input.
pub fn decode<'a>(input: &'a [u8], limits: &Limits) -> Result<Value<'a>, Error> {
    let (value, used) = decode_prefix(input, limits)?;
    if used != input.len() {
        return Err(Error {
            kind: ErrorKind::TrailingBytes,
            pos: used,
        });
    }
    Ok(value)
}

/// Decodes one value from the start of `input` and returns it together with
/// the number of bytes it occupied. Bytes after the value are not examined.
pub fn decode_prefix<'a>(input: &'a [u8], limits: &Limits) -> Result<(Value<'a>, usize), Error> {
    Decoder {
        input,
        pos: 0,
        items: 0,
        limits: *limits,
    }
    .run()
}

enum Frame<'a> {
    List {
        items: Vec<Value<'a>>,
    },
    Dict {
        builder: DictBuilder<'a>,
        key: Option<(&'a [u8], usize)>,
    },
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    items: usize,
    limits: Limits,
}

impl<'a> Decoder<'a> {
    fn err<T>(&self, kind: ErrorKind) -> Result<T, Error> {
        Err(Error {
            kind,
            pos: self.pos,
        })
    }

    fn peek(&self) -> Result<u8, Error> {
        match self.input.get(self.pos) {
            Some(b) => Ok(*b),
            None => self.err(ErrorKind::UnexpectedEof),
        }
    }

    fn advance(&mut self, n: usize) {
        // Every caller passes a constant small step while `pos` is within
        // the input, so this cannot overflow in practice; saturating keeps
        // that promise total — a saturated position simply decodes as
        // `UnexpectedEof` below instead of panicking.
        self.pos = self.pos.saturating_add(n);
    }

    fn count_item(&mut self) -> Result<(), Error> {
        self.items = self.items.checked_add(1).ok_or(Error {
            kind: ErrorKind::TooManyItems,
            pos: self.pos,
        })?;
        if self.items > self.limits.max_items {
            return self.err(ErrorKind::TooManyItems);
        }
        Ok(())
    }

    fn run(mut self) -> Result<(Value<'a>, usize), Error> {
        let mut stack: Vec<(Frame<'a>, usize)> = Vec::new();

        loop {
            // Peek once per iteration and reuse it for the dict-close,
            // list-close and value-dispatch checks below.
            let b = self.peek()?;
            // Depth for the key gate below, read before the mutable
            // borrow of the frame (keys are values at depth + 1).
            let key_depth = matches!(stack.last(), Some((Frame::Dict { key: None, .. }, _)))
                .then(|| stack.len());
            // Inside a dictionary with no pending key: read a key or the end.
            if let Some((
                Frame::Dict {
                    key: key @ None,
                    builder,
                },
                start,
            )) = stack.last_mut()
            {
                let start = *start;
                if b == b'e' {
                    self.advance(1);
                    let builder = std::mem::take(builder);
                    stack.pop();
                    let value = Value::Dict(builder.finish());
                    if let Some(done) = self.emit(&mut stack, value, start)? {
                        return Ok(done);
                    }
                    continue;
                }
                if !b.is_ascii_digit() {
                    return self.err(ErrorKind::NonStringKey);
                }
                // Keys are gated like any other scalar at their depth.
                if key_depth.is_some_and(|depth| depth >= self.limits.max_depth) {
                    return self.err(ErrorKind::TooDeep);
                }
                let key_pos = self.pos;
                let k = self.parse_bytes()?;
                self.count_item()?;
                if !builder.check_key(k) {
                    return Err(Error {
                        kind: ErrorKind::DuplicateKey,
                        pos: key_pos,
                    });
                }
                *key = Some((k, self.pos));
                continue;
            }

            // Inside a list: the end marker closes it.
            if let Some((Frame::List { items }, start)) = stack.last_mut()
                && b == b'e'
            {
                let start = *start;
                self.advance(1);
                let items = std::mem::take(items);
                stack.pop();
                if let Some(done) = self.emit(&mut stack, Value::List(items), start)? {
                    return Ok(done);
                }
                continue;
            }

            // A value begins here. Containers push a frame; scalars are gated
            // by the same depth (their depth is stack.len() + 1).
            let start = self.pos;
            let value = match b {
                b'i' => {
                    if stack.len() >= self.limits.max_depth {
                        return self.err(ErrorKind::TooDeep);
                    }
                    let v = self.parse_int()?;
                    self.count_item()?;
                    Value::Int(v)
                }
                b'0'..=b'9' => {
                    if stack.len() >= self.limits.max_depth {
                        return self.err(ErrorKind::TooDeep);
                    }
                    let b = self.parse_bytes()?;
                    self.count_item()?;
                    Value::Bytes(b)
                }
                open @ (b'l' | b'd') => {
                    if stack.len() >= self.limits.max_depth {
                        return self.err(ErrorKind::TooDeep);
                    }
                    self.count_item()?;
                    self.advance(1);
                    let frame = if open == b'l' {
                        Frame::List { items: Vec::new() }
                    } else {
                        Frame::Dict {
                            builder: DictBuilder::new(),
                            key: None,
                        }
                    };
                    stack.push((frame, start));
                    continue;
                }
                other => return self.err(ErrorKind::UnexpectedByte(other)),
            };
            if let Some(done) = self.emit(&mut stack, value, start)? {
                return Ok(done);
            }
        }
    }

    /// Hands a finished value to its parent. Returns the final result once the
    /// top-level value is complete.
    fn emit(
        &mut self,
        stack: &mut [(Frame<'a>, usize)],
        value: Value<'a>,
        start: usize,
    ) -> Result<Option<(Value<'a>, usize)>, Error> {
        match stack.last_mut() {
            None => Ok(Some((value, self.pos))),
            Some((Frame::List { items }, _)) => {
                items.push(value);
                Ok(None)
            }
            Some((Frame::Dict { builder, key }, _)) => match key.take() {
                Some((k, value_start)) => {
                    let raw = self.input.get(value_start..self.pos).ok_or(Error {
                        kind: ErrorKind::UnexpectedEof,
                        pos: start,
                    })?;
                    builder.push(k, value, raw);
                    Ok(None)
                }
                // A value can only be emitted into a dict after its key was read.
                None => Err(Error {
                    kind: ErrorKind::NonStringKey,
                    pos: start,
                }),
            },
        }
    }

    /// Parses digits up to (not including) `terminator`, rejecting leading
    /// zeros and overflow. Returns the magnitude.
    fn parse_digits(&mut self, terminator: u8, kind: ErrorKind) -> Result<u64, Error> {
        let digits_start = self.pos;
        let mut value: u64 = 0;
        loop {
            let b = self.peek()?;
            if b == terminator {
                break;
            }
            if !b.is_ascii_digit() {
                return self.err(kind);
            }
            if self.pos > digits_start && value == 0 {
                // A leading zero followed by more digits ("03").
                return self.err(kind);
            }
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(b.wrapping_sub(b'0'))))
                .ok_or(Error {
                    kind: ErrorKind::IntegerOverflow,
                    pos: self.pos,
                })?;
            self.advance(1);
        }
        if self.pos == digits_start {
            return self.err(kind);
        }
        Ok(value)
    }

    fn parse_int(&mut self) -> Result<i64, Error> {
        self.advance(1); // 'i'
        let negative = self.peek()? == b'-';
        if negative {
            self.advance(1);
        }
        let magnitude = self.parse_digits(b'e', ErrorKind::InvalidInteger)?;
        self.advance(1); // 'e'
        if negative {
            if magnitude == 0 {
                return Err(Error {
                    kind: ErrorKind::InvalidInteger,
                    pos: self.pos,
                });
            }
            // -(2^63) is the most negative i64.
            let limit = (i64::MAX as u64).saturating_add(1);
            if magnitude > limit {
                return Err(Error {
                    kind: ErrorKind::IntegerOverflow,
                    pos: self.pos,
                });
            }
            Ok(0i64.wrapping_sub_unsigned(magnitude))
        } else {
            i64::try_from(magnitude).map_err(|_| Error {
                kind: ErrorKind::IntegerOverflow,
                pos: self.pos,
            })
        }
    }

    fn parse_bytes(&mut self) -> Result<&'a [u8], Error> {
        let len = self
            .parse_digits(b':', ErrorKind::InvalidLength)
            .map_err(|e| {
                if e.kind == ErrorKind::IntegerOverflow {
                    Error {
                        kind: ErrorKind::StringTooLong,
                        pos: e.pos,
                    }
                } else {
                    e
                }
            })?;
        self.advance(1); // ':'
        let len = usize::try_from(len).map_err(|_| Error {
            kind: ErrorKind::StringTooLong,
            pos: self.pos,
        })?;
        if len > self.limits.max_string_len {
            return self.err(ErrorKind::StringTooLong);
        }
        let end = self.pos.checked_add(len).ok_or(Error {
            kind: ErrorKind::UnexpectedEof,
            pos: self.pos,
        })?;
        let bytes = match self.input.get(self.pos..end) {
            Some(b) => b,
            None => return self.err(ErrorKind::UnexpectedEof),
        };
        self.pos = end;
        Ok(bytes)
    }
}
