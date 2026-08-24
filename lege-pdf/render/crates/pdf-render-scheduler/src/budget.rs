//! Byte-denominated admission control for in-flight jobs.
//!
//! A permit is acquired for a job's estimated peak bytes before work starts
//! and released on drop. Blocking `acquire` parks the requesting worker
//! until enough bytes free up — backpressure, not failure — while requests
//! exceeding the whole budget fail fast.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How long a blocked `acquire_cancellable` waits before re-testing the
/// caller's cancellation flag. Short enough that a cancelled pipeline drains
/// promptly, long enough that an uncontended wait is still a plain park.
const CANCEL_POLL: Duration = Duration::from_millis(25);

#[derive(Debug)]
struct State {
    in_use: u64,
}

#[derive(Debug)]
struct Inner {
    limit: u64,
    state: Mutex<State>,
    freed: Condvar,
}

/// Shared memory budget. Cheap to clone.
#[derive(Debug, Clone)]
pub struct MemoryBudget {
    inner: Arc<Inner>,
}

/// Error for requests that can never succeed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("requested {requested} bytes exceeds total budget {limit}")]
pub struct BudgetExceeded {
    pub requested: u64,
    pub limit: u64,
}

impl MemoryBudget {
    pub fn new(limit: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                limit,
                state: Mutex::new(State { in_use: 0 }),
                freed: Condvar::new(),
            }),
        }
    }

    pub fn limit(&self) -> u64 {
        self.inner.limit
    }

    /// Lock with poison recovery: `State` is a single `u64` mutated only by
    /// non-panicking arithmetic, so a poisoned lock still holds valid data
    /// (a panicking worker cannot leave it torn) — recover, never panic.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn in_use(&self) -> u64 {
        self.lock_state().in_use
    }

    /// Block until `bytes` can be reserved. Fails fast if `bytes` exceeds
    /// the total budget (it could never be satisfied).
    pub fn acquire(&self, bytes: u64) -> Result<MemoryPermit, BudgetExceeded> {
        match self.acquire_cancellable(bytes, || false)? {
            Some(permit) => Ok(permit),
            // `cancelled` is constantly false, so the wait only ends with a
            // permit. Report the reservation as unsatisfiable rather than
            // panicking on an impossible branch.
            None => Err(BudgetExceeded {
                requested: bytes,
                limit: self.inner.limit,
            }),
        }
    }

    /// Blocking `acquire` that also gives up when `cancelled` becomes true.
    ///
    /// A worker parked on the condvar is woken only by a `release`, so without
    /// this a cancelled pipeline still holds every blocked worker until some
    /// *other* job finishes — the cancellation is honoured late, and on a
    /// pipeline whose remaining jobs are all queued behind the budget, only
    /// once the in-flight ones drain. Re-testing the flag on a short timeout
    /// costs nothing on the uncontended path and bounds that delay.
    ///
    /// Returns `Ok(None)` when the wait was abandoned because of cancellation.
    pub fn acquire_cancellable(
        &self,
        bytes: u64,
        cancelled: impl Fn() -> bool,
    ) -> Result<Option<MemoryPermit>, BudgetExceeded> {
        if bytes > self.inner.limit {
            return Err(BudgetExceeded {
                requested: bytes,
                limit: self.inner.limit,
            });
        }
        if cancelled() {
            return Ok(None);
        }
        let mut state = self.lock_state();
        while bytes > self.inner.limit - state.in_use {
            // Same poison-recovery rationale as `lock_state`.
            let (next, _timeout) = self
                .inner
                .freed
                .wait_timeout(state, CANCEL_POLL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if cancelled() {
                return Ok(None);
            }
        }
        state.in_use += bytes;
        Ok(Some(MemoryPermit {
            budget: self.clone(),
            bytes,
        }))
    }

    /// Non-blocking variant; `None` if the reservation doesn't fit now.
    pub fn try_acquire(&self, bytes: u64) -> Result<Option<MemoryPermit>, BudgetExceeded> {
        if bytes > self.inner.limit {
            return Err(BudgetExceeded {
                requested: bytes,
                limit: self.inner.limit,
            });
        }
        let mut state = self.lock_state();
        if bytes > self.inner.limit - state.in_use {
            return Ok(None);
        }
        state.in_use += bytes;
        Ok(Some(MemoryPermit {
            budget: self.clone(),
            bytes,
        }))
    }

    fn release(&self, bytes: u64) {
        let mut state = self.lock_state();
        state.in_use = state.in_use.saturating_sub(bytes);
        drop(state);
        self.inner.freed.notify_all();
    }
}

/// RAII reservation of budget bytes.
#[derive(Debug)]
pub struct MemoryPermit {
    budget: MemoryBudget,
    bytes: u64,
}

impl MemoryPermit {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn permits_account_and_release() {
        let b = MemoryBudget::new(100);
        let p1 = b.acquire(60).unwrap();
        assert_eq!(b.in_use(), 60);
        assert!(b.try_acquire(50).unwrap().is_none());
        drop(p1);
        assert_eq!(b.in_use(), 0);
        assert!(b.try_acquire(50).unwrap().is_some());
    }

    #[test]
    fn oversized_request_fails_fast() {
        let b = MemoryBudget::new(10);
        assert!(b.acquire(11).is_err());
    }

    #[test]
    fn blocked_acquire_wakes_on_release() {
        let b = MemoryBudget::new(100);
        let p = b.acquire(80).unwrap();
        let b2 = b.clone();
        let waiter = thread::spawn(move || {
            let _p = b2.acquire(50).unwrap();
            b2.in_use()
        });
        thread::sleep(Duration::from_millis(50));
        drop(p);
        assert_eq!(waiter.join().unwrap(), 50);
    }

    #[test]
    fn admission_check_does_not_overflow_at_u64_limit() {
        let b = MemoryBudget::new(u64::MAX);
        let _almost_all = b.acquire(u64::MAX - 1).unwrap();
        assert!(b.try_acquire(2).unwrap().is_none());
        assert_eq!(b.in_use(), u64::MAX - 1);
    }
}
