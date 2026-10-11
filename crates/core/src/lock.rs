//! The ONE mutex poisoning policy of the runtime.
//!
//! A poisoned `std::sync::Mutex` records that a holder panicked; whether that
//! is recoverable depends on what the lock protects:
//!
//! - **Ephemeral state recovers.** In-memory coordination state (registries,
//!   caches, queues, counters) is a *projection* reconciled from durable
//!   authorities; a panicking holder leaves it at worst stale, so
//!   [`recover`] clears the poison flag and hands back the inner value. One
//!   panicking caller can never wedge the runtime.
//! - **Durable authorities surface typed unavailable.** A lock that gates a
//!   durable authority (a store writer, a journal head, any object whose
//!   bytes are the record of truth) must NOT be silently entered with
//!   possibly-inconsistent state: [`acquire`] refuses with
//!   [`LockUnavailable`] naming the authority, and recovery is the owner's
//!   explicit restart/reconciliation decision.
//!
//! Direct `.lock().unwrap()` is therefore never the policy: it panics, which
//! converts an availability incident into a crash. Pick [`recover`] for
//! ephemeral state or [`acquire`] for durable authorities.

use std::fmt;
use std::sync::{Mutex, MutexGuard};

/// Typed refusal from [`acquire`]: the durable authority's mutex is poisoned
/// and deliberately was not entered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockUnavailable {
    /// Logical name of the durable authority that is unavailable.
    pub authority: &'static str,
}

impl fmt::Display for LockUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "durable authority `{}` is unavailable: its lock is poisoned by a panicking holder",
            self.authority
        )
    }
}

impl std::error::Error for LockUnavailable {}

impl From<LockUnavailable> for crate::Error {
    fn from(e: LockUnavailable) -> Self {
        crate::Error::new(crate::ErrorKind::Internal, e.to_string())
    }
}

/// Ephemeral-state policy: recover a poisoned guard with the poison flag
/// cleared. The guarded value is reconciled from durable authorities, so the
/// worst case is a stale projection, never lost truth.
#[inline]
pub fn recover<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}

/// Durable-authority policy: refuse a poisoned lock with a typed
/// [`LockUnavailable`] instead of entering with possibly-inconsistent state.
#[inline]
pub fn acquire<'a, T>(
    lock: &'a Mutex<T>,
    authority: &'static str,
) -> Result<MutexGuard<'a, T>, LockUnavailable> {
    lock.lock().map_err(|_| LockUnavailable { authority })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poison<T>(lock: &Mutex<T>) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.lock().unwrap();
            panic!("planted panic while holding the lock");
        }));
    }

    /// Ephemeral state recovers: a poisoned lock yields the inner value and
    /// the poison flag is cleared, so a later acquire succeeds normally.
    /// Never panics.
    #[test]
    fn recover_survives_poison_and_clears_it() {
        let lock = Mutex::new(7u32);
        poison(&lock);
        assert!(lock.is_poisoned());

        let guard = recover(&lock);
        assert_eq!(*guard, 7, "recovered guard must expose the inner value");
        drop(guard);

        assert!(
            !lock.is_poisoned(),
            "recover must clear the poison flag for subsequent holders"
        );
        assert_eq!(*recover(&lock), 7);
    }

    /// Durable authorities surface a typed error: acquire refuses a poisoned
    /// lock, names the authority, and never panics.
    #[test]
    fn acquire_refuses_poison_with_typed_error() {
        let lock = Mutex::new("durable".to_string());
        poison(&lock);

        let err = acquire(&lock, "test.authority").expect_err("poisoned lock must be refused");
        assert_eq!(err.authority, "test.authority");
        assert!(err.to_string().contains("test.authority"));
        assert!(
            lock.is_poisoned(),
            "refusal must not clear the poison flag: only the owner decides recovery"
        );
        let core_err: crate::Error = err.into();
        assert_eq!(core_err.kind, crate::ErrorKind::Internal);
    }

    /// The healthy path of acquire returns the guard.
    #[test]
    fn acquire_returns_guard_when_healthy() {
        let lock = Mutex::new(3u32);
        let guard = acquire(&lock, "test.authority").expect("healthy lock must be acquired");
        assert_eq!(*guard, 3);
    }
}
