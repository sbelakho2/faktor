//! Test-harness memory guard: a defensive, attributed bound on what ONE
//! `cargo test -p faktor-cli` process may hold live at once.
//!
//! CI pipeline 48 OOM-killed the cli bin test binary (`faktor_cli-…`) at
//! `anon-rss` ~30 GiB with NO attribution — libtest only reports a killed
//! signal. The guard wraps the process-wide allocator for the bin test
//! target: every live byte is counted and crossing the cap prints the exact
//! test thread (libtest names test threads after their tests) plus the live
//! and limit byte counts to stderr, then aborts — a loud, attributed failure
//! instead of a silent kernel OOM kill.
//!
//! * `FAKTOR_TEST_MEM_LIMIT_BYTES` overrides the cap (0 disables it).
//! * The default cap is 4 GiB of LIVE bytes: far above every legitimate
//!   single-test allocation in this suite (the largest explicit test
//!   buffers are single-digit MiB; whole-suite baselines stay under 1 GiB)
//!   and far below the 16/32 GiB runaway that this guard exists to stop.
//! * The guard never allocates: the message is rendered into a stack buffer
//!   and written with `libc::write`, so it cannot recurse into itself.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Default live-byte cap for the test binary (4 GiB). Deliberately orders of
/// magnitude above any legitimate test allocation and below the CI runaway.
pub const DEFAULT_TEST_MEMORY_LIMIT_BYTES: usize = 4 * 1024 * 1024 * 1024;

/// Live bytes currently held by the process (allocations minus frees).
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Resolved cap; `usize::MAX` means disabled.
static LIMIT_BYTES: AtomicUsize = AtomicUsize::new(usize::MAX);
/// One-shot env resolution flag.
static LIMIT_READY: AtomicBool = AtomicBool::new(false);
/// One-shot trip flag: only the first crossing reports.
static TRIPPED: AtomicBool = AtomicBool::new(false);

/// `#[global_allocator]` for the bin test binary only (`#[cfg(test)]`).
pub struct GuardedAllocator;

fn configured_limit() -> usize {
    if !LIMIT_READY.swap(true, Ordering::SeqCst) {
        let raw = std::env::var("FAKTOR_TEST_MEM_LIMIT_BYTES");
        let parsed = match raw {
            Ok(text) => match text.trim().parse::<usize>() {
                Ok(0) => usize::MAX,
                Ok(value) => value,
                Err(_) => DEFAULT_TEST_MEMORY_LIMIT_BYTES,
            },
            Err(_) => DEFAULT_TEST_MEMORY_LIMIT_BYTES,
        };
        LIMIT_BYTES.store(parsed, Ordering::SeqCst);
    }
    LIMIT_BYTES.load(Ordering::SeqCst)
}

fn record_allocation(size: usize) {
    let previous = LIVE_BYTES.fetch_add(size, Ordering::Relaxed);
    let live = previous.saturating_add(size);
    let limit = configured_limit();
    if limit != usize::MAX && live > limit && !TRIPPED.swap(true, Ordering::SeqCst) {
        report_and_abort(live, limit);
    }
}

fn report_and_abort(live: usize, limit: usize) {
    let name = std::thread::current()
        .name()
        .unwrap_or("<unknown thread>")
        .chars()
        .take(200)
        .collect::<String>();
    let mut buffer = [0u8; 512];
    let mut writer = StackWriter {
        buffer: &mut buffer,
        len: 0,
    };
    let _ = write!(
        writer,
        "\n[faktor-test-memory-guard] live={live} bytes exceeds limit={limit} bytes \
         in test thread `{name}`; aborting before the kernel OOM-kills silently. \
         Raise it with FAKTOR_TEST_MEM_LIMIT_BYTES (0 disables) only if the test \
         legitimately needs more.\n"
    );
    let text = writer.text();
    let mut written = 0;
    while written < text.len() {
        // SAFETY: `text` is a live local buffer and fd 2 is the process
        // stderr; `write` never allocates, so the allocator path cannot
        // recurse here.
        let n = unsafe {
            libc::write(
                2,
                text[written..].as_ptr() as *const libc::c_void,
                text.len() - written,
            )
        };
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
    // The trip is one-shot (TRIPPED is already set), so the allocations the
    // backtrace machinery makes on this failure path cannot re-trip the
    // guard; the capture names the allocating call stack in the abort log.
    let backtrace = std::backtrace::Backtrace::force_capture();
    let _ = write!(std::io::stderr(), "{backtrace}");
    std::process::abort();
}

/// Fixed-capacity `fmt::Write` sink that never allocates.
struct StackWriter<'a> {
    buffer: &'a mut [u8],
    len: usize,
}

impl StackWriter<'_> {
    fn text(&self) -> &[u8] {
        &self.buffer[..self.len]
    }
}

impl std::fmt::Write for StackWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = self.buffer.len() - self.len;
        let take = room.min(s.len());
        self.buffer[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

// SAFETY: `GlobalAlloc` is an inherently unsafe trait; this wrapper only
// observes allocation sizes and forwards every operation verbatim to
// `System`, which upholds the allocator contract.
unsafe impl GlobalAlloc for GuardedAllocator {
    // SAFETY: forwards the caller's layout to `System.alloc` unchanged and
    // only records the requested size on success.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    // SAFETY: forwards caller-obtained pointers to `System.alloc_zeroed`
    // unchanged and only records the requested size on success.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc_zeroed(layout);
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    // SAFETY: the pointer/layout pair is the one the caller obtained from
    // this allocator (which forwarded it to `System`); the counter update
    // cannot touch the allocation.
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(pointer, layout);
    }

    // SAFETY: the pointer/layout pair is the one the caller obtained from
    // this allocator; the counter update cannot touch the allocation and the
    // resize is forwarded to `System.realloc` unchanged.
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let resized = System.realloc(pointer, layout, new_size);
        if !resized.is_null() && new_size > layout.size() {
            record_allocation(new_size - layout.size());
        } else if !resized.is_null() {
            LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
        }
        resized
    }
}
