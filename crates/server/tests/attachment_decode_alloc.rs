//! Single-allocation proof for the strict-protocol attachment base64 decode
//! (audit item 15): a max-size CANONICAL payload must decode directly from
//! `data_base64`'s own bytes into exactly ONE pre-sized destination, with no
//! compacted/whitespace-stripped intermediate copy. The counting allocator
//! is process-global, and this binary intentionally holds a single test so
//! the measurement cannot be polluted by parallel tests.

#![allow(unsafe_code)] // platform authority module: every unsafe
                       // block/function in this module carries a `// SAFETY:` justification and is
                       // enumerated by tests/static-authority.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use faktor_server::native::{decode_attachment_base64, MAX_ATTACHMENT_UPLOAD_BYTES};

struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
            ALLOCS.fetch_add(1, Ordering::SeqCst);
        }
        ptr
    }

    // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
            ALLOCS.fetch_add(1, Ordering::SeqCst);
        }
        ptr
    }

    // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn max_size_canonical_payload_decodes_in_one_allocation_and_whitespace_is_refused() {
    // The max-size decode: exactly the daemon's decoded-byte upload ceiling.
    let raw = vec![0x5au8; MAX_ATTACHMENT_UPLOAD_BYTES];
    let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);

    let baseline_live = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline_live, Ordering::SeqCst);
    let baseline_allocs = ALLOCS.load(Ordering::SeqCst);
    let decoded = decode_attachment_base64(encoded.as_bytes()).expect("canonical payload decodes");
    let live_growth = LIVE.load(Ordering::SeqCst).saturating_sub(baseline_live);
    let peak_growth = PEAK.load(Ordering::SeqCst).saturating_sub(baseline_live);
    let allocs = ALLOCS
        .load(Ordering::SeqCst)
        .saturating_sub(baseline_allocs);

    assert_eq!(decoded.len(), MAX_ATTACHMENT_UPLOAD_BYTES);
    assert_eq!(decoded, raw, "the decoded bytes are byte-exact");
    assert_eq!(
        allocs, 1,
        "the decode path must allocate exactly ONE destination buffer, saw {allocs}"
    );
    assert_eq!(
        live_growth, MAX_ATTACHMENT_UPLOAD_BYTES,
        "the single allocation is exactly the decoded length (no compact/intermediate copy)"
    );
    assert_eq!(
        peak_growth, MAX_ATTACHMENT_UPLOAD_BYTES,
        "no peak above the one destination buffer (saw {peak_growth})"
    );

    // Whitespace and non-canonical variants are TYPED refusals, never a
    // tolerated decode. (Measured after the allocation assertion.)
    let spaced = format!("{}\n", encoded);
    let err = decode_attachment_base64(spaced.as_bytes()).expect_err("whitespace refused");
    assert_eq!(err.http_status, 400);
    assert!(err.message.contains("whitespace"), "{err:?}");
    let unpadded = encoded.trim_end_matches('=');
    let err = decode_attachment_base64(unpadded.as_bytes()).expect_err("non-canonical padding");
    assert_eq!(err.http_status, 400);
    assert!(err.message.contains("multiple of 4"), "{err:?}");
}
