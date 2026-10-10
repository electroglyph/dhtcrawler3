//! Small helpers shared by the modules.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// Locks `m`, recovering the data if another thread panicked while holding it.
/// Every critical section in this crate leaves its data consistent at each
/// step, so a poisoned lock is still safe to use.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `now + d` for deadlines and expiries. On overflow (only possible with
/// absurd durations) it returns `now`, so the affected deadline or TTL ends
/// at once (fail closed). Skip and pacing wakes must use [`after_skip`]
/// instead, where waking at once would be fail open.
pub(crate) fn after(now: Instant, d: Duration) -> Instant {
    now.checked_add(d).unwrap_or(now)
}

/// `now + d` for sampler skip-wakes and pacing wakes (next eligible or
/// free-at time). On overflow (only possible with absurd tunings) it
/// saturates ~100 years out, so the node stays skipped instead of becoming
/// eligible at once.
pub(crate) fn after_skip(now: Instant, d: Duration) -> Instant {
    // ~100 years in seconds; far beyond any process lifetime, and far below
    // what could overflow a monotonic clock reading near boot.
    const FAR_FUTURE_SKIP: Duration = Duration::from_secs(3_153_600_000);
    now.checked_add(d).unwrap_or_else(|| {
        // `d` was near `Duration::MAX`. The inner fallback needs `now`
        // within ~100 years of the clock maximum, which a live process
        // cannot observe (clocks start near boot, range spans eons).
        now.checked_add(FAR_FUTURE_SKIP).unwrap_or(now)
    })
}

/// What's left of a shared pre-send wait budget after `elapsed` was spent
/// waiting: per-address spacing and budget acquisition share one
/// `max_send_wait`, so the budget wait only gets the remainder instead of
/// a second full `max_send_wait`.
pub(crate) fn remaining_budget(max_wait: Duration, elapsed: Duration) -> Duration {
    max_wait.checked_sub(elapsed).unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_budget_shares_one_wait_bound() {
        let max = Duration::from_secs(4);
        assert_eq!(
            remaining_budget(max, Duration::ZERO),
            Duration::from_secs(4)
        );
        assert_eq!(
            remaining_budget(max, Duration::from_secs(1)),
            Duration::from_secs(3)
        );
        assert_eq!(remaining_budget(max, max), Duration::ZERO);
        assert_eq!(
            remaining_budget(max, max + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn skip_overflow_stays_skipped_far_future() {
        let now = Instant::now();
        let wake = after_skip(now, Duration::MAX);
        // ~100-year saturation: still skipped a year out ...
        assert!(wake > now + Duration::from_secs(31_536_000));
        // ... while the deadline helper expires at once for the same input.
        assert_eq!(after(now, Duration::MAX), now);
    }
}
