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
