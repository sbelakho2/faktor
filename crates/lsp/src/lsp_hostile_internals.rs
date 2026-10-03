//! Adversarial boundaries of the LSP client internals.
//!
//! The stderr ring (byte-exact eviction and lossy tails), the `file://` URI
//! encoder, the bounded stdin writer queue (a wedged server), and the
//! delivery-unknown/timeout classification shared with MCP. Each row asserts
//! one exact outcome with a message naming the row.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use faktor_core::error::ErrorKind;

/// Drains until released, then accepts everything. `started` is set the first
/// time a write enters the blocking section, so tests can pin the exact
/// claim-before-cancel window without sleeping.
#[derive(Clone, Default)]
struct GatedSink {
    started: Arc<AtomicBool>,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

impl GatedSink {
    fn release(&self) {
        let (lock, cond) = &*self.gate;
        *lock.lock().unwrap() = true;
        cond.notify_all();
    }
    fn wait_started(&self, bound: Duration) {
        let deadline = std::time::Instant::now() + bound;
        while !self.started.load(Ordering::SeqCst) {
            assert!(
                std::time::Instant::now() < deadline,
                "the writer must claim the frame within the bound"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl std::io::Write for GatedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.started.store(true, Ordering::SeqCst);
        let (lock, cond) = &*self.gate;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = cond.wait(released).unwrap();
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct RingRow {
    label: &'static str,
    cap: usize,
    pushes: Vec<Vec<u8>>,
    expect_tail: Vec<u8>,
    expect_total: u64,
}

fn ring_rows() -> Vec<RingRow> {
    let mut rows = Vec::new();
    let mut push = |label, cap, pushes: Vec<&[u8]>, expect_tail: &[u8], expect_total: u64| {
        rows.push(RingRow {
            label,
            cap,
            pushes: pushes.iter().map(|p| p.to_vec()).collect(),
            expect_tail: expect_tail.to_vec(),
            expect_total,
        })
    };
    push("cap-8-under", 8, vec![b"abc".as_slice()], b"abc", 3);
    push(
        "cap-8-exact",
        8,
        vec![b"abcdefgh".as_slice()],
        b"abcdefgh",
        8,
    );
    push(
        "cap-8-over-one",
        8,
        vec![b"abcdefghi".as_slice()],
        b"bcdefghi",
        9,
    );
    push(
        "cap-8-two-pushes-exact",
        8,
        vec![b"abcd".as_slice(), b"efgh".as_slice()],
        b"abcdefgh",
        8,
    );
    push(
        "cap-8-two-pushes-over",
        8,
        vec![b"abcd".as_slice(), b"efghi".as_slice()],
        b"bcdefghi",
        9,
    );
    push("cap-1-one", 1, vec![b"x".as_slice()], b"x", 1);
    push("cap-1-two", 1, vec![b"xy".as_slice()], b"y", 2);
    push(
        "cap-1-many-pushes",
        1,
        vec![b"a".as_slice(), b"b".as_slice(), b"c".as_slice()],
        b"c",
        3,
    );
    push("cap-0-any", 0, vec![b"abc".as_slice()], b"", 3);
    push("cap-0-empty", 0, vec![b"".as_slice()], b"", 0);
    push(
        "cap-4-chunk-equals-cap",
        4,
        vec![b"wxyz".as_slice()],
        b"wxyz",
        4,
    );
    push(
        "cap-4-chunk-greater-than-cap",
        4,
        vec![b"0123456789".as_slice()],
        b"6789",
        10,
    );
    push(
        "cap-4-chunk-greater-then-small",
        4,
        vec![b"0123456789".as_slice(), b"ab".as_slice()],
        b"89ab",
        12,
    );
    push(
        "cap-4-small-then-greater",
        4,
        vec![b"ab".as_slice(), b"0123456789".as_slice()],
        b"6789",
        12,
    );
    push(
        "cap-16-many-small",
        16,
        vec![
            b"a".as_slice(),
            b"bb".as_slice(),
            b"ccc".as_slice(),
            b"dddd".as_slice(),
            b"eeeee".as_slice(),
        ],
        b"abbcccddddeeeee",
        15,
    );
    push(
        "cap-6-many-small-over",
        6,
        vec![b"aaaa".as_slice(), b"bbbb".as_slice(), b"cccc".as_slice()],
        b"bbcccc",
        12,
    );
    push(
        "cap-10-newlines",
        10,
        vec![
            b"line1\n".as_slice(),
            b"line2\n".as_slice(),
            b"line3\n".as_slice(),
        ],
        b"ne2\nline3\n".as_slice(),
        18,
    );
    push(
        "cap-3-invalid-utf8-tail",
        3,
        vec![&[0xff, 0xfe, 0xfd, b'a'][..]],
        &[0xfe, 0xfd, b'a'],
        4,
    );
    rows
}

/// Every stderr-ring row asserts the exact retained tail bytes and the exact
/// total-bytes counter; a single chunk larger than the cap keeps only its own
/// tail, and byte caps never exceed the configured cap.
#[test]
fn stderr_ring_boundary_matrix_is_byte_exact() {
    for row in ring_rows() {
        let mut ring = crate::StderrRing::new(row.cap);
        for chunk in &row.pushes {
            ring.push(chunk);
        }
        let tail = ring.tail_lossy();
        let expected = String::from_utf8_lossy(&row.expect_tail).into_owned();
        assert_eq!(
            tail, expected,
            "case {:?}: the retained tail must be the trailing min(cap, pushed) bytes",
            row.label
        );
        assert!(
            row.expect_tail.len() <= row.cap,
            "case {:?}: the ring must never retain more than its cap",
            row.label
        );
        assert_eq!(
            ring.total, row.expect_total,
            "case {:?}: total drained bytes must count every pushed byte",
            row.label
        );
    }
}

/// Invalid UTF-8 in the tail renders through lossy replacement (never a panic
/// or a silent drop).
#[test]
fn stderr_ring_lossy_tail_renders_invalid_utf8() {
    let mut ring = crate::StderrRing::new(8);
    ring.push(&[b'a', 0xff, b'b']);
    let tail = ring.tail_lossy();
    assert!(
        tail.starts_with('a') && tail.ends_with('b'),
        "the lossy tail must preserve the valid frame bytes: {tail:?}"
    );
    assert!(
        tail.contains('\u{fffd}'),
        "invalid bytes must render as the replacement character: {tail:?}"
    );
    assert_eq!(ring.total, 3, "all three bytes are counted");
}

/// Percent-encoding corpus for the `file://` URI builder: unreserved bytes,
/// `/` and `:` pass through; every other byte is `%XX` uppercase.
#[test]
fn file_uri_percent_encoding_corpus() {
    let rows: [(&str, &str, &str); 18] = [
        ("simple", "/tmp/ws", "file:///tmp/ws"),
        ("space", "/a b", "file:///a%20b"),
        ("hash", "/a#b", "file:///a%23b"),
        ("question", "/a?b", "file:///a%3Fb"),
        ("percent", "/a%b", "file:///a%25b"),
        ("plus", "/a+b", "file:///a%2Bb"),
        ("at", "/a@b", "file:///a%40b"),
        ("equals", "/a=b", "file:///a%3Db"),
        ("amp", "/a&b", "file:///a%26b"),
        ("semicolon", "/a;b", "file:///a%3Bb"),
        ("brackets", "/a[]b", "file:///a%5B%5Db"),
        ("newline", "/a\nb", "file:///a%0Ab"),
        ("tab", "/a\tb", "file:///a%09b"),
        ("backslash", "/a\\b", "file:///a%5Cb"),
        ("colon-kept", "/C:/x", "file:///C:/x"),
        ("tilde-kept", "/~user", "file:///~user"),
        ("dots-kept", "/a.b_c-d", "file:///a.b_c-d"),
        ("unicode", "/caf\u{e9}", "file:///caf%C3%A9"),
    ];
    for (label, path, expected) in rows {
        assert_eq!(
            crate::file_uri(Path::new(path)),
            expected,
            "case {label}: file URI encoding must be byte-exact"
        );
    }
}

/// The stdin writer queue is bounded: once the server is wedged, the queue
/// fills and further enqueues are typed `Oversized` refusals. The writer
/// thread stays alive (blocked on the sink) and only ends after release.
#[test]
fn stdin_writer_queue_full_is_a_typed_oversized_refusal() {
    let sink = GatedSink::default();
    let handle = crate::spawn_stdin_writer(sink.clone());
    // First frame: the writer claims it and blocks in the sink.
    let (tx1, _rx1) = std::sync::mpsc::sync_channel(1);
    handle
        .enqueue(
            b"first".to_vec(),
            crate::WriterAck::Sync(tx1),
            std::sync::Arc::new(crate::FrameState::queued()),
            "probe-1",
        )
        .expect("the first enqueue must fit");
    sink.wait_started(Duration::from_secs(5));
    // The queue holds exactly WRITER_QUEUE_CAP frames now.
    let cap = 64usize;
    for i in 0..cap {
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        handle
            .enqueue(
                format!("queued-{i}").into_bytes(),
                crate::WriterAck::Sync(tx),
                std::sync::Arc::new(crate::FrameState::queued()),
                "probe-full",
            )
            .unwrap_or_else(|e| panic!("queued frame {i} must fit the {cap}-slot queue: {e}"));
    }
    let (tx, _rx) = std::sync::mpsc::sync_channel(1);
    let err = handle
        .enqueue(
            b"overflow".to_vec(),
            crate::WriterAck::Sync(tx),
            std::sync::Arc::new(crate::FrameState::queued()),
            "probe-overflow",
        )
        .expect_err("one frame past the queue cap must be refused");
    assert_eq!(
        err.kind,
        ErrorKind::Oversized,
        "a full queue is a typed oversized refusal (never a blocked caller): {err}"
    );
    assert!(
        err.message.contains("queue is full"),
        "the refusal must name the full queue: {err}"
    );
    // Release the wedged sink and let the writer drain; the thread exits on
    // channel disconnect.
    sink.release();
    drop(handle);
}

/// A frame whose write had ALREADY STARTED when the deadline expired is
/// delivery-unknown, never a clean timeout (a blind retry could double-apply
/// the request).
#[test]
fn wedged_write_deadline_is_delivery_unknown() {
    let sink = GatedSink::default();
    let handle = crate::spawn_stdin_writer(sink.clone());
    let probe = handle.clone();
    let writer = std::thread::spawn(move || {
        probe.write_and_flush_bounded(b"frame".to_vec(), Duration::from_millis(200), "wedged")
    });
    sink.wait_started(Duration::from_secs(5));
    let err = writer
        .join()
        .unwrap()
        .expect_err("the wedged write must time out");
    assert!(
        faktor_mcp::is_delivery_unknown(&err),
        "a write that had begun is delivery-unknown, never a clean timeout: {err}"
    );
    assert_ne!(
        err.kind,
        ErrorKind::Timeout,
        "delivery-unknown must not be shaped as a retryable timeout"
    );
    sink.release();
    drop(handle);
}

/// The LSP stdout framing is the shared MCP Content-Length parser: the same
/// corpus classifies identically here (one contract, no divergent parser).
#[test]
fn lsp_framing_is_the_shared_mcp_contract() {
    let cases: [(&str, &[u8], bool); 8] = [
        ("complete", b"Content-Length: 2\r\n\r\n{}", true),
        ("incomplete", b"Content-Length: 5\r\n\r\n{}", false),
        ("invalid-json", b"Content-Length: 1\r\n\r\nx", false),
        ("missing-length", b"X: 1\r\n\r\n{}", false),
        (
            "oversized-declared",
            b"Content-Length: 999999999\r\n\r\n",
            false,
        ),
        ("negative", b"Content-Length: -1\r\n\r\n", false),
        ("empty", b"", false),
        ("valid-object", b"Content-Length: 2\r\n\r\n{}", true),
    ];
    for (label, bytes, complete) in cases {
        let parsed = faktor_mcp::parse_frame(bytes);
        if complete {
            let (consumed, _) = parsed
                .unwrap_or_else(|e| panic!("case {label}: expected a complete frame: {e}"))
                .unwrap_or_else(|| panic!("case {label}: expected a complete frame"));
            assert_eq!(
                consumed,
                bytes.len(),
                "case {label}: the shared parser consumes the whole frame"
            );
        } else {
            assert!(
                !matches!(parsed, Ok(Some(_))),
                "case {label}: must not parse as a complete frame"
            );
        }
    }
}
