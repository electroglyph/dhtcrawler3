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

/// `now + d`. On overflow (only possible with absurd durations) it returns
/// `now`, which makes the affected deadline or skip period end at once.
pub(crate) fn after(now: Instant, d: Duration) -> Instant {
    now.checked_add(d).unwrap_or(now)
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
}
