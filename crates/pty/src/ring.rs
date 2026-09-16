//! Bounded output ring shared by both PTY backends (the unix reader thread
//! and the Windows ConPTY reader thread). Oldest bytes are dropped so memory
//! stays bounded regardless of how much output a hostile child produces.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

/// Bounded output ring (bytes kept, oldest dropped) — RAM stays bounded
/// for hostile or huge output.
pub(crate) const RING_MAX_BYTES: usize = 256 * 1024;

pub(crate) struct Ring {
    buf: VecDeque<u8>,
    total: u64,
}

impl Ring {
    pub(crate) fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            total: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.buf.push_back(*b);
        }
        while self.buf.len() > RING_MAX_BYTES {
            self.buf.pop_front();
        }
        self.total = self.total.saturating_add(bytes.len() as u64);
    }

    /// Drain the currently available bytes.
    pub(crate) fn drain(&mut self) -> Vec<u8> {
        self.buf.drain(..).collect()
    }

    pub(crate) fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    /// Total bytes ever pushed (before ring eviction).
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Classified RING => REBUILD: drop every retained byte and reset the
    /// counter. The ring is a bounded LOSSY output buffer, never a
    /// correctness authority (the durable terminal rows are), so a torn
    /// ring is rebuilt instead of propagated.
    pub(crate) fn rebuild(&mut self) {
        self.buf.clear();
        self.total = 0;
    }
}

/// Lock the output ring with classified recovery: a poisoned guard (a
/// panicking reader that held the ring) is recovered with the poison flag
/// cleared and the ring REBUILT empty — bytes are honestly lost, and one
/// panicked reader can never wedge every later snapshot/drain/push.
pub(crate) fn lock_ring(ring: &Mutex<Ring>) -> MutexGuard<'_, Ring> {
    ring.lock().unwrap_or_else(|poisoned| {
        ring.clear_poison();
        let mut guard = poisoned.into_inner();
        guard.rebuild();
        guard
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_ring_is_rebuilt_not_propagated() {
        let ring = Mutex::new(Ring::new());
        {
            let mut guard = ring.lock().unwrap();
            guard.push(b"pending bytes");
            assert_eq!(guard.total(), 13);
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = ring.lock().unwrap();
            guard.push(b"boom");
            panic!("reader thread poisoned the ring");
        }));
        assert!(ring.is_poisoned());
        // Classified ring => rebuild: the torn ring is rebuilt EMPTY (the
        // lossy output buffer is never a correctness authority) and the
        // poison flag is cleared, so later snapshots/drains keep working.
        let mut guard = lock_ring(&ring);
        assert_eq!(guard.total(), 0);
        assert!(guard.snapshot().is_empty());
        guard.push(b"ok");
        assert_eq!(guard.snapshot(), b"ok");
        drop(guard);
        assert!(!ring.is_poisoned());
        assert_eq!(lock_ring(&ring).total(), 2);
    }
}
