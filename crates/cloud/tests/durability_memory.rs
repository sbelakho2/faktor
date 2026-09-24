//! Bounded-memory proof for the commercial content fingerprint (P1 audit
//! finding): hashing a large table must stream row-by-row instead of
//! materializing the database in Rust heap. The counting allocator is
//! process-global, and this binary intentionally holds a single test so the
//! measurement cannot be polluted by parallel tests.

#![allow(unsafe_code)] // platform authority module: every unsafe
                       // block/function in this module carries a `// SAFETY:` justification and is
                       // enumerated by tests/static-authority.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::Connection;

struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the `GlobalAlloc` contract is honored exactly: `layout` is passed through unchanged and the returned pointer is the allocator's.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(live, Ordering::SeqCst);
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

const ROWS: i64 = 80_000;
const PAYLOAD_BYTES: usize = 512;
/// The table holds ~41 MiB of value bytes; a whole-DB materialization would
/// need at least that much Rust heap. Streaming must stay far below it.
const HEAP_CEILING: usize = 8 * 1024 * 1024;

#[test]
fn fingerprint_of_a_large_table_streams_within_a_bounded_heap() {
    let conn = Connection::open_in_memory().unwrap();
    faktor_cloud::durability::apply_policy(&conn).unwrap();
    conn.execute_batch("CREATE TABLE events (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);")
        .unwrap();
    let payload = vec![0x5au8; PAYLOAD_BYTES];
    {
        let tx = conn.unchecked_transaction().unwrap();
        let mut insert = tx
            .prepare("INSERT INTO events (id, payload) VALUES (?1, ?2)")
            .unwrap();
        for id in 0..ROWS {
            insert.execute(rusqlite::params![id, &payload]).unwrap();
        }
        drop(insert);
        tx.commit().unwrap();
    }

    let baseline = LIVE.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let fingerprint = faktor_cloud::durability::canonical_fingerprint(&conn).unwrap();
    let peak_growth = PEAK.load(Ordering::SeqCst).saturating_sub(baseline);

    assert_eq!(fingerprint.rows, ROWS, "every row is covered");
    assert_eq!(fingerprint.tables, 1);
    assert!(
        peak_growth < HEAP_CEILING,
        "content fingerprint must stream: peak Rust-heap growth {peak_growth} bytes \
         while hashing {} bytes of table content",
        ROWS as usize * PAYLOAD_BYTES
    );
}
