//! Bookkeeping for downloading metadata pieces: pipelining, ordering,
//! duplicate and length checks.

use crate::{FetchError, MAX_PIPELINED_REQUESTS, METADATA_PIECE_LEN};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PieceState {
    NotRequested,
    Requested,
    Received,
}

/// Collects `ut_metadata` pieces for a metadata blob of a known size.
///
/// Received pieces are stored individually, so memory grows only with data
/// the peer actually sent, not with the size it advertised.
#[derive(Debug)]
pub(crate) struct Assembly {
    size: usize,
    state: Vec<PieceState>,
    pieces: Vec<Option<Vec<u8>>>,
    /// Lowest index that may still be `NotRequested`.
    next: usize,
    outstanding: usize,
    received: usize,
}

impl Assembly {
    /// `size` must already be validated to be non-zero and within limits.
    pub(crate) fn new(size: usize) -> Result<Self, FetchError> {
        if size == 0 {
            return Err(FetchError::MetadataSizeInvalid(0));
        }
        let count = size.div_ceil(METADATA_PIECE_LEN);
        Ok(Self {
            size,
            state: vec![PieceState::NotRequested; count],
            pieces: vec![None; count],
            next: 0,
            outstanding: 0,
            received: 0,
        })
    }

    fn piece_count(&self) -> usize {
        self.state.len()
    }

    /// Exact length that piece `index` must have.
    fn expected_len(&self, index: usize) -> usize {
        let start = index.saturating_mul(METADATA_PIECE_LEN);
        self.size.saturating_sub(start).min(METADATA_PIECE_LEN)
    }

    /// Marks and returns the pieces to request now, keeping at most
    /// [`MAX_PIPELINED_REQUESTS`] outstanding.
    pub(crate) fn next_requests(&mut self) -> Vec<usize> {
        let mut out = Vec::new();
        while self.outstanding < MAX_PIPELINED_REQUESTS {
            let Some(state) = self.state.get_mut(self.next) else {
                break;
            };
            if *state == PieceState::NotRequested {
                *state = PieceState::Requested;
                out.push(self.next);
                self.outstanding = self.outstanding.saturating_add(1);
            }
            self.next = self.next.saturating_add(1);
        }
        out
    }

    /// Checks one data message without storing it, and returns its piece
    /// index. The same checks run again in [`accept`](Self::accept).
    pub(crate) fn validate(
        &self,
        piece: i64,
        total_size: i64,
        data: &[u8],
    ) -> Result<usize, FetchError> {
        if usize::try_from(total_size).ok() != Some(self.size) {
            return Err(FetchError::Protocol(format!(
                "total_size {total_size} differs from metadata_size {}",
                self.size
            )));
        }
        let index = usize::try_from(piece)
            .ok()
            .filter(|i| *i < self.piece_count())
            .ok_or_else(|| FetchError::Protocol(format!("piece {piece} out of range")))?;
        let expected = self.expected_len(index);
        if data.len() != expected {
            return Err(FetchError::Protocol(format!(
                "piece {index} has {} bytes, expected {expected}",
                data.len()
            )));
        }
        if self.state.get(index) == Some(&PieceState::Received) {
            return Err(FetchError::Protocol(format!("duplicate piece {index}")));
        }
        Ok(index)
    }

    /// Accepts one data message.
    pub(crate) fn accept(
        &mut self,
        piece: i64,
        total_size: i64,
        data: &[u8],
    ) -> Result<(), FetchError> {
        let index = self.validate(piece, total_size, data)?;
        let state = self
            .state
            .get_mut(index)
            .ok_or_else(|| FetchError::Protocol(format!("piece {piece} out of range")))?;
        match *state {
            PieceState::Received => {
                return Err(FetchError::Protocol(format!("duplicate piece {index}")));
            }
            PieceState::Requested => self.outstanding = self.outstanding.saturating_sub(1),
            PieceState::NotRequested => {}
        }
        *state = PieceState::Received;
        if let Some(slot) = self.pieces.get_mut(index) {
            *slot = Some(data.to_vec());
        }
        self.received = self.received.saturating_add(1);
        Ok(())
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.received == self.piece_count()
    }

    /// Concatenates the pieces. Call only when [`is_complete`](Self::is_complete).
    pub(crate) fn finish(self) -> Result<Vec<u8>, FetchError> {
        let mut out = Vec::with_capacity(self.size);
        for piece in self.pieces {
            let piece = piece.ok_or_else(|| FetchError::Protocol("missing piece".into()))?;
            out.extend_from_slice(&piece);
        }
        if out.len() != self.size {
            return Err(FetchError::Protocol("assembled size mismatch".into()));
        }
        Ok(out)
    }
}
