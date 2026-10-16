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
        self.total = self.total.saturating_add(bytes.len() as u64);
        if bytes.len() >= RING_MAX_BYTES {
            // One offer at or beyond the whole ring: only its own tail can
            // survive. Replace the buffer with that tail in ONE bulk move
            // instead of per-byte push/pop churn.
            self.buf.clear();
            self.buf
                .extend(bytes[bytes.len() - RING_MAX_BYTES..].iter().copied());
            return;
        }
        self.buf.extend(bytes.iter().copied());
        let excess = self.buf.len().saturating_sub(RING_MAX_BYTES);
        if excess > 0 {
            self.buf.drain(..excess);
        }
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

    /// Bounded deterministic workload: 4 MiB offered in 64 KiB chunks — the
    /// exact shape a PTY reader thread sees during a large build's output.
    /// Content is asserted exactly (retained tail, oversized single offer,
    /// empty offer) and the elapsed time is printed for the audit report.
    #[test]
    fn bulk_push_retains_the_exact_tail() {
        let mut ring = Ring::new();
        let chunk: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        let started = std::time::Instant::now();
        for _ in 0..64 {
            ring.push(&chunk);
        }
        let elapsed = started.elapsed();
        eprintln!("pty-ring: 4 MiB in 64 KiB pushes = {elapsed:?}");
        assert_eq!(ring.total(), 64 * 64 * 1024);
        let mut expected = Vec::new();
        for _ in 0..(RING_MAX_BYTES / chunk.len()) {
            expected.extend_from_slice(&chunk);
        }
        assert_eq!(ring.snapshot(), expected);
        assert_eq!(ring.drain(), expected);
        assert_eq!(ring.snapshot(), Vec::<u8>::new());

        // One offer LARGER than the ring keeps exactly its own tail.
        let big: Vec<u8> = (0..(RING_MAX_BYTES + 4096))
            .map(|i| (i % 241) as u8)
            .collect();
        ring.push(&big);
        assert_eq!(ring.snapshot(), big[4096..].to_vec());
        assert_eq!(ring.total(), (64 * 64 * 1024 + big.len()) as u64);

        // An empty offer changes nothing; the total still counts what was
        // offered.
        ring.push(&[]);
        assert_eq!(ring.snapshot(), big[4096..].to_vec());
        assert_eq!(ring.total(), (64 * 64 * 1024 + big.len()) as u64);

        // Boundary: exact capacity, then one more byte.
        let mut boundary = Ring::new();
        boundary.push(&big[..RING_MAX_BYTES]);
        assert_eq!(boundary.snapshot().len(), RING_MAX_BYTES);
        boundary.push(b"z");
        let mut expected_boundary = big[1..RING_MAX_BYTES].to_vec();
        expected_boundary.push(b'z');
        assert_eq!(boundary.snapshot(), expected_boundary);
    }

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
