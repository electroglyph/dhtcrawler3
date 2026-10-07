//! Bookkeeping for downloading metadata pieces: pipelining, ordering,
//! duplicate and length checks.

use std::time::{Duration, Instant};

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
    /// When each piece was last requested; `None` until first requested.
    /// Parallel to [`Self::state`].
    requested_at: Vec<Option<Instant>>,
    /// Lowest index that may still be `NotRequested`.
    next: usize,
    outstanding: usize,
    received: usize,
}

/// Largest piece count an [`Assembly`] tracks, whatever the fetch limit
/// allows: 4096 pieces of 16 KiB (64 MiB). The tables stay small even if
/// `max_metadata` is ever raised; the default 8 MiB limit needs about 512.
const MAX_ASSEMBLY_PIECES: usize = 4096;

/// How long a requested piece may go unanswered before it is asked for
/// again. Short enough for several rounds inside the default 30 s fetch
/// budget while other pieces flow, long enough not to duplicate requests
/// to a merely slow peer.
const PIECE_RETRY_AFTER: Duration = Duration::from_secs(2);

impl Assembly {
    /// `size` must already be validated to be non-zero and within limits.
    /// Sizes needing more than [`MAX_ASSEMBLY_PIECES`] pieces are refused
    /// here too, so the tables below stay small whatever the caller allows.
    pub(crate) fn new(size: usize) -> Result<Self, FetchError> {
        if size == 0 {
            return Err(FetchError::MetadataSizeInvalid(0));
        }
        let count = size.div_ceil(METADATA_PIECE_LEN);
        if count > MAX_ASSEMBLY_PIECES {
            return Err(FetchError::MetadataSizeInvalid(
                i64::try_from(size).unwrap_or(i64::MAX),
            ));
        }
        Ok(Self {
            size,
            state: vec![PieceState::NotRequested; count],
            pieces: vec![None; count],
            requested_at: vec![None; count],
            next: 0,
            outstanding: 0,
            received: 0,
        })
    }

    fn piece_count(&self) -> usize {
        self.state.len()
    }

    /// True when `piece` names a piece of this metadata.
    pub(crate) fn has_piece(&self, piece: i64) -> bool {
        usize::try_from(piece)
            .ok()
            .is_some_and(|i| i < self.piece_count())
    }

    /// Exact length that piece `index` must have.
    fn expected_len(&self, index: usize) -> usize {
        let start = index.saturating_mul(METADATA_PIECE_LEN);
        self.size.saturating_sub(start).min(METADATA_PIECE_LEN)
    }

    /// Marks and returns the pieces to request now, keeping at most
    /// [`MAX_PIPELINED_REQUESTS`] outstanding.
    ///
    /// Pieces requested longer than [`PIECE_RETRY_AFTER`] ago without an
    /// answer go out again first: a peer that silently drops one response
    /// while other traffic flows would otherwise stall the fetch, since
    /// every read still meets its own deadline and `next` never looks back.
    /// A re-emitted piece stays `Requested`, so `outstanding` is untouched.
    pub(crate) fn next_requests(&mut self) -> Vec<usize> {
        let mut out = Vec::new();
        let now = Instant::now();
        for index in 0..self.state.len() {
            if out.len() >= MAX_PIPELINED_REQUESTS {
                break;
            }
            if self.state[index] == PieceState::Requested && self.is_expired(index, now) {
                self.requested_at[index] = Some(now);
                out.push(index);
            }
        }
        while self.outstanding < MAX_PIPELINED_REQUESTS && out.len() < MAX_PIPELINED_REQUESTS {
            let Some(state) = self.state.get_mut(self.next) else {
                break;
            };
            if *state == PieceState::NotRequested {
                *state = PieceState::Requested;
                self.requested_at[self.next] = Some(now);
                out.push(self.next);
                self.outstanding = self.outstanding.saturating_add(1);
            }
            self.next = self.next.saturating_add(1);
        }
        out
    }

    /// True when piece `index` was requested at least [`PIECE_RETRY_AFTER`]
    /// ago. A missing stamp means never requested, which never expires.
    fn is_expired(&self, index: usize, now: Instant) -> bool {
        self.requested_at
            .get(index)
            .copied()
            .flatten()
            .is_some_and(|at| now.duration_since(at) >= PIECE_RETRY_AFTER)
    }

    /// True when `piece` names a piece we asked for and still await: a
    /// reject for any other piece is unsolicited and ignored.
    pub(crate) fn is_awaiting(&self, piece: i64) -> bool {
        usize::try_from(piece)
            .ok()
            .and_then(|i| self.state.get(i))
            .is_some_and(|s| *s == PieceState::Requested)
    }

    /// Checks one data message without storing it, and returns its piece
    /// index. The same checks run again in [`accept`](Self::accept).
    /// A redundant copy of an already-received piece is acceptable: `accept`
    /// ignores it, so validation does too (idempotent, not an error).
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
        Ok(index)
    }

    /// Accepts one data message. A redundant copy of an already-received
    /// piece is ignored (first copy wins): our own retry re-emits expired
    /// `Requested` pieces without touching `outstanding`, so a merely-slow
    /// peer may answer both the original and the retry, and that must not
    /// fail the fetch. Integrity still rests on the final hash check.
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
            // Already stored: ignore the redundant copy without touching
            // `outstanding` (already decremented) or `received`.
            PieceState::Received => return Ok(()),
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
        // Fallible: an abort on reserve would turn a bad size into a crash.
        let mut out = Vec::new();
        out.try_reserve(self.size)
            .map_err(|_| FetchError::Protocol("assembled metadata too large".into()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A stamp far enough in the past that [`is_expired`](Assembly::is_expired)
    /// holds. Tests backdate stamps instead of sleeping.
    fn long_ago() -> Instant {
        Instant::now()
            .checked_sub(PIECE_RETRY_AFTER)
            .and_then(|t| t.checked_sub(Duration::from_secs(1)))
            .expect("test clock went backwards")
    }

    /// Backdates every request stamp past [`PIECE_RETRY_AFTER`], as if the
    /// peer silently dropped every outstanding response.
    fn expire_all(a: &mut Assembly) {
        let past = long_ago();
        for slot in a.requested_at.iter_mut().flatten() {
            *slot = past;
        }
    }

    #[test]
    fn dropped_piece_is_retried_until_the_fetch_completes() {
        let size = 5 * METADATA_PIECE_LEN + 10;
        let mut a = Assembly::new(size).unwrap();
        assert_eq!(a.next_requests(), vec![0, 1, 2, 3]);
        let total = size as i64;
        let full = vec![7u8; METADATA_PIECE_LEN];
        // The peer answers everything except piece 1: no data, no reject.
        for i in [0, 2, 3] {
            a.accept(i, total, &full).unwrap();
        }
        assert_eq!(a.next_requests(), vec![4, 5]);
        a.accept(4, total, &full).unwrap();
        a.accept(5, total, &[9u8; 10]).unwrap();
        assert!(!a.is_complete());
        // Nothing left to ask for, and the missing piece never timed out a
        // read on its own: without a retry this fetch never completes.
        assert_eq!(a.next_requests(), Vec::<usize>::new());
        // Past the retry horizon the dropped piece goes out again ...
        expire_all(&mut a);
        assert_eq!(a.next_requests(), vec![1]);
        // ... and this time the peer answers.
        a.accept(1, total, &full).unwrap();
        assert!(a.is_complete());
    }

    #[test]
    fn expired_requests_reemit_without_touching_outstanding() {
        let size = 5 * METADATA_PIECE_LEN + 10;
        let mut a = Assembly::new(size).unwrap();
        assert_eq!(a.next_requests(), vec![0, 1, 2, 3]);
        assert_eq!(a.outstanding, 4);
        expire_all(&mut a);
        assert_eq!(a.next_requests(), vec![0, 1, 2, 3]);
        // Still one unanswered request per piece: the re-emit neither lost
        // nor duplicated the pipeline accounting.
        assert_eq!(a.outstanding, 4);
        // The re-emit refreshed the stamps, so there is no retry storm: the
        // very next call asks for nothing.
        assert_eq!(a.next_requests(), Vec::<usize>::new());
    }

    #[test]
    fn received_pieces_are_never_reemitted() {
        let size = 3 * METADATA_PIECE_LEN;
        let mut a = Assembly::new(size).unwrap();
        assert_eq!(a.next_requests(), vec![0, 1, 2]);
        let total = size as i64;
        let full = vec![7u8; METADATA_PIECE_LEN];
        a.accept(1, total, &full).unwrap();
        expire_all(&mut a);
        // Index order, and the answered piece stays answered.
        assert_eq!(a.next_requests(), vec![0, 2]);
    }

    #[test]
    fn retries_go_first_and_fresh_fills_remaining_slots() {
        let size = 5 * METADATA_PIECE_LEN + 10;
        let mut a = Assembly::new(size).unwrap();
        assert_eq!(a.next_requests(), vec![0, 1, 2, 3]);
        let total = size as i64;
        let full = vec![7u8; METADATA_PIECE_LEN];
        a.accept(0, total, &full).unwrap();
        // Only piece 1 times out; 4 and 5 are still fresh.
        a.requested_at[1] = Some(long_ago());
        assert_eq!(a.next_requests(), vec![1, 4]);
        assert_eq!(a.next_requests(), Vec::<usize>::new());
    }

    #[test]
    fn duplicate_data_is_ignored_without_touching_accounting() {
        let size = 3 * METADATA_PIECE_LEN;
        let mut a = Assembly::new(size).unwrap();
        assert_eq!(a.next_requests(), vec![0, 1, 2]);
        let total = size as i64;
        let full = vec![7u8; METADATA_PIECE_LEN];
        a.accept(0, total, &full).unwrap();
        let (outstanding, received) = (a.outstanding, a.received);
        // Redundant copy (original + retry both answered): ignored, and the
        // pipeline accounting is untouched.
        a.accept(0, total, &full).unwrap();
        assert_eq!((a.outstanding, a.received), (outstanding, received));
        // The first copy wins: a conflicting second copy cannot corrupt the
        // assembly (the final hash check is the integrity gate).
        let mut other = vec![7u8; METADATA_PIECE_LEN];
        other[0] ^= 0x01;
        a.accept(0, total, &other).unwrap();
        assert_eq!(a.pieces[0].as_ref().unwrap(), &full);
        assert!(!a.is_complete());
    }

    #[test]
    fn retry_sweep_is_capped_at_pipeline_depth() {
        let size = 7 * METADATA_PIECE_LEN + 10;
        let mut a = Assembly::new(size).unwrap();
        assert_eq!(a.next_requests(), vec![0, 1, 2, 3]);
        let total = size as i64;
        let full = vec![7u8; METADATA_PIECE_LEN];
        a.accept(0, total, &full).unwrap();
        a.accept(1, total, &full).unwrap();
        assert_eq!(a.next_requests(), vec![4, 5]);
        expire_all(&mut a);
        // At most one re-request per pipeline slot per call.
        assert_eq!(a.next_requests(), vec![2, 3, 4, 5]);
        assert_eq!(a.outstanding, 4);
    }
}
