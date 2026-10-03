//! Adversarial framing corpus for the MCP Content-Length parser and the
//! per-frame write state machine.
//!
//! Every row drives the production `parse_frame` (or `FrameState`) with one
//! byte-level hostile input and asserts its exact outcome class with a
//! message naming the row. The corpus covers truncation at every header
//! boundary, malformed lengths, duplicate/missing headers, encoding faults,
//! oversized declarations, concatenated frames and JSON shape variants.

use super::*;

#[derive(Debug, PartialEq)]
enum Outcome {
    /// Incomplete: wait for more bytes, never a parse error.
    Incomplete,
    /// Unrecoverable frame violation.
    Error,
    /// A complete frame: consumed length plus the decoded JSON value.
    Frame(usize, serde_json::Value),
}

fn classify(bytes: &[u8]) -> Outcome {
    match parse_frame(bytes) {
        Ok(None) => Outcome::Incomplete,
        Err(_) => Outcome::Error,
        Ok(Some((consumed, value))) => Outcome::Frame(consumed, value),
    }
}

struct Row {
    label: &'static str,
    bytes: Vec<u8>,
    expect: Outcome,
}

fn row(label: &'static str, bytes: impl Into<Vec<u8>>, expect: Outcome) -> Row {
    Row {
        label,
        bytes: bytes.into(),
        expect,
    }
}

fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn rows() -> Vec<Row> {
    let mut rows = Vec::new();
    let ok = |body: &str| Outcome::Frame(frame(body).len(), serde_json::from_str(body).unwrap());

    // --- empty / truncation at every boundary -----------------------------
    rows.push(row("empty-buffer", Vec::new(), Outcome::Incomplete));
    rows.push(row("whitespace-only", b" \r\n".to_vec(), Outcome::Error));
    rows.push(row(
        "no-terminator",
        b"Content-Length: 5\r\n".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "terminator-without-body",
        b"Content-Length: 5\r\n\r\n".to_vec(),
        Outcome::Incomplete,
    ));
    rows.push(row(
        "body-one-short",
        b"Content-Length: 5\r\n\r\nhell".to_vec(),
        Outcome::Incomplete,
    ));
    rows.push(row(
        "zero-length-body",
        b"Content-Length: 0\r\n\r\n".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "lf-only-header",
        b"Content-Length: 5\n\nhello".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "cr-only-header",
        b"Content-Length: 5\r\rhello".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "terminator-split-across-reads-is-retried-as-is",
        b"\r\n\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));

    // --- malformed / hostile Content-Length values -------------------------
    for (label, value) in [
        ("length-negative", "-1"),
        ("length-plus-sign-accepted-as-five", "+5"),
        ("length-float", "5.0"),
        ("length-hex", "0x5"),
        ("length-empty", ""),
        ("length-space-only", " "),
        ("length-letters", "abc"),
        ("length-u64-overflow", "99999999999999999999"),
        ("length-usize-overflow-64-digits", &"9".repeat(64)[..]),
        ("length-huge-mib", "999999999"),
        ("length-nul", "5\0"),
        ("length-comma", "1,2"),
    ] {
        let expect = if value == "+5" {
            // Rust's integer parser accepts a leading '+': the declared
            // length is 5 and the buffer is short, so the frame is simply
            // incomplete (parity with the standard parser, not a bypass).
            Outcome::Incomplete
        } else {
            Outcome::Error
        };
        rows.push(row(
            label,
            format!("Content-Length: {value}\r\n\r\n{{}}").into_bytes(),
            expect,
        ));
    }
    rows.push(row(
        "declared-over-16mib-bound",
        format!("Content-Length: {}\r\n\r\n", MAX_RESPONSE_BYTES + 1).into_bytes(),
        Outcome::Error,
    ));
    rows.push(row(
        "declared-exactly-16mib-bound-incomplete",
        format!("Content-Length: {}\r\n\r\n", MAX_RESPONSE_BYTES).into_bytes(),
        Outcome::Incomplete,
    ));
    rows.push(row(
        "whole-buffer-over-16mib",
        vec![b'x'; MAX_RESPONSE_BYTES + 1],
        Outcome::Error,
    ));
    rows.push(row(
        "declared-but-buffer-over-bound-first",
        {
            let mut v = b"Content-Length: 1\r\n\r\n".to_vec();
            v.extend_from_slice(&vec![b'x'; MAX_RESPONSE_BYTES + 1]);
            v
        },
        Outcome::Error,
    ));

    // --- header syntax variants -------------------------------------------
    rows.push(row(
        "header-lowercase-refused",
        b"content-length: 2\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "header-mixed-case-refused",
        b"Content-length: 2\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "header-with-leading-space-refused",
        b" Content-Length: 2\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));
    let ok_bytes = |bytes: Vec<u8>| Outcome::Frame(bytes.len(), serde_json::json!({}));
    rows.push(row(
        "header-extra-space-after-colon",
        b"Content-Length:  2\r\n\r\n{}".to_vec(),
        ok_bytes(b"Content-Length:  2\r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "header-tab-after-colon",
        b"Content-Length:\t2\r\n\r\n{}".to_vec(),
        ok_bytes(b"Content-Length:\t2\r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "header-trailing-space",
        b"Content-Length: 2 \r\n\r\n{}".to_vec(),
        ok_bytes(b"Content-Length: 2 \r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "header-leading-zeros",
        b"Content-Length: 02\r\n\r\n{}".to_vec(),
        ok_bytes(b"Content-Length: 02\r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "duplicate-length-last-wins-in-mcp",
        b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        ok_bytes(b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "duplicate-conflicting-last-wins-used",
        b"Content-Length: 99\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        ok_bytes(b"Content-Length: 99\r\nContent-Length: 2\r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "other-headers-ignored",
        b"X-Trace: abc\r\nContent-Length: 2\r\nX-More: 1\r\n\r\n{}".to_vec(),
        ok_bytes(b"X-Trace: abc\r\nContent-Length: 2\r\nX-More: 1\r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "bare-colon-line-ignored",
        b":\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        ok_bytes(b":\r\nContent-Length: 2\r\n\r\n{}".to_vec()),
    ));
    rows.push(row(
        "header-not-utf8",
        b"Content-Length\xff: 2\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "header-with-nul-before-terminator",
        b"Content-Length: 2\0\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "header-missing-terminator-with-long-garbage",
        {
            let mut v = vec![b'h'; 8192];
            v.extend_from_slice(b"\r\n\r\n{}");
            v
        },
        Outcome::Error,
    ));

    // --- body encoding and JSON shape --------------------------------------
    rows.push(row(
        "body-invalid-utf8",
        {
            let mut v = b"Content-Length: 2\r\n\r\n".to_vec();
            v.extend_from_slice(&[0xff, 0xfe]);
            v
        },
        Outcome::Error,
    ));
    rows.push(row("body-invalid-json", frame("{not json"), Outcome::Error));
    rows.push(row("body-json-object", frame("{}"), ok("{}")));
    rows.push(row(
        "body-json-array",
        frame("[1,2,3]"),
        Outcome::Frame(frame("[1,2,3]").len(), serde_json::json!([1, 2, 3])),
    ));
    rows.push(row(
        "body-json-string",
        frame("\"hello\""),
        Outcome::Frame(frame("\"hello\"").len(), serde_json::json!("hello")),
    ));
    rows.push(row(
        "body-json-number",
        frame("42"),
        Outcome::Frame(frame("42").len(), serde_json::json!(42)),
    ));
    rows.push(row(
        "body-json-null",
        frame("null"),
        Outcome::Frame(frame("null").len(), serde_json::Value::Null),
    ));
    rows.push(row(
        "body-json-bool",
        frame("true"),
        Outcome::Frame(frame("true").len(), serde_json::json!(true)),
    ));
    rows.push(row(
        "body-with-escaped-nul-in-string",
        frame("\"\\u0000\""),
        Outcome::Frame(frame("\"\\u0000\"").len(), serde_json::json!("\0")),
    ));
    rows.push(row(
        "body-leading-whitespace-is-valid-json",
        frame("  {}  "),
        Outcome::Frame(frame("  {}  ").len(), serde_json::json!({})),
    ));
    {
        let depth = 64;
        let body = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let bytes = frame(&body);
        let consumed = bytes.len();
        let value = serde_json::from_str(&body).unwrap();
        rows.push(row(
            "body-deep-nesting-within-bound",
            bytes,
            Outcome::Frame(consumed, value),
        ));
    }
    rows.push(row(
        "body-deeper-than-json-recursion-limit",
        {
            let depth = 100_000;
            let body = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            frame(&body)
        },
        Outcome::Error,
    ));

    // --- concatenation and trailing bytes ----------------------------------
    let one = frame("{}");
    let mut doubled = one.clone();
    doubled.extend_from_slice(&one);
    rows.push(row(
        "two-frames-consumes-first",
        doubled,
        Outcome::Frame(one.len(), serde_json::json!({})),
    ));
    rows.push(row(
        "declared-shorter-than-body-consumes-prefix",
        {
            let mut v = b"Content-Length: 2\r\n\r\n{}trailing".to_vec();
            v.truncate(v.len());
            v
        },
        Outcome::Frame(b"Content-Length: 2\r\n\r\n{}".len(), serde_json::json!({})),
    ));
    rows.push(row(
        "declared-longer-than-body-is-incomplete",
        b"Content-Length: 100\r\n\r\n{}".to_vec(),
        Outcome::Incomplete,
    ));
    rows.push(row(
        "bom-before-header",
        b"\xef\xbb\xbfContent-Length: 2\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "garbage-before-header",
        b"GARBAGE Content-Length: 2\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));
    rows.push(row(
        "extra-crlf-after-header",
        b"Content-Length: 2\r\n\r\n\r\n{}".to_vec(),
        Outcome::Error,
    ));

    rows
}

/// Every row asserts its exact outcome class (and for frames, the consumed
/// length and decoded value) with a message naming the row.
#[test]
fn parse_frame_hostile_corpus_matches_every_outcome() {
    let rows = rows();
    assert!(
        rows.len() >= 50,
        "the framing corpus must keep at least 50 rows, found {}",
        rows.len()
    );
    for r in rows {
        let got = classify(&r.bytes);
        match (&r.expect, &got) {
            (Outcome::Incomplete, Outcome::Incomplete) => {}
            (Outcome::Error, Outcome::Error) => {}
            (Outcome::Frame(want_len, want_val), Outcome::Frame(got_len, got_val)) => {
                assert_eq!(
                    got_len, want_len,
                    "case {:?}: consumed byte count must be exact",
                    r.label
                );
                assert_eq!(
                    got_val, want_val,
                    "case {:?}: decoded JSON must match",
                    r.label
                );
            }
            _ => panic!(
                "case {:?}: outcome mismatch (want {:?}, got {:?})",
                r.label,
                r.expect,
                classify(&r.bytes)
            ),
        }
    }
}

/// The frame state machine: exactly one of the racing transitions wins, and
/// a frame the writer claimed can never be cancelled (its delivery is
/// unknown). Drop of a guard cancels a still-queued frame.
#[test]
fn frame_state_transitions_are_race_exact() {
    // begin_write wins exactly once.
    let s = FrameState::queued();
    assert!(s.begin_write(), "first writer claim must win");
    assert!(!s.begin_write(), "second writer claim must lose");
    assert!(
        !s.cancel_before_write(),
        "a claimed frame can never be cancelled"
    );

    // cancel wins exactly once, then the writer skips the frame whole.
    let s = FrameState::queued();
    assert!(s.cancel_before_write(), "first cancel must win");
    assert!(!s.cancel_before_write(), "second cancel must lose");
    assert!(
        !s.begin_write(),
        "the writer must skip a frame cancelled before the claim"
    );

    // A guard dropped while queued cancels; dropped after the claim it is a
    // no-op.
    {
        let guard = FrameState::guarded();
        assert!(guard.state().cancel_before_write(), "guard state is queued");
    }
    {
        let guard = FrameState::guarded();
        assert!(
            guard.state().begin_write(),
            "the writer can claim a guarded frame"
        );
        let state = guard.state().clone();
        drop(guard);
        assert!(
            !state.cancel_before_write(),
            "dropping the guard after the claim must not cancel"
        );
    }
}

/// `join_thread_bounded` returns true for a finished thread and false for one
/// still running at the bound (never blocking past it).
#[test]
fn join_thread_bounded_respects_the_bound() {
    let handle = std::thread::spawn(|| {});
    assert!(
        join_thread_bounded(handle, Duration::from_secs(5)),
        "a finished thread joins true"
    );
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let _ = rx.recv_timeout(Duration::from_secs(10));
    });
    let start = std::time::Instant::now();
    let joined = join_thread_bounded(handle, Duration::from_millis(50));
    assert!(
        !joined,
        "a thread still running at the bound must report false"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "the bounded join must not block far past its bound: {:?}",
        start.elapsed()
    );
    let _ = tx.send(());
}

/// The delivery-unknown marker is matched by its stable error code, never by
/// message text, and is never retryable.
#[test]
fn delivery_unknown_is_typed_by_code_and_not_retryable() {
    let err = delivery_unknown("mcp tools/call");
    assert!(
        is_delivery_unknown(&err),
        "the minted error must classify as delivery-unknown"
    );
    assert_eq!(
        err.kind,
        ErrorKind::Provider {
            code: DELIVERY_UNKNOWN_CODE.into(),
            retryable: false,
        },
        "delivery-unknown must carry the stable code and never be retryable"
    );
    assert!(
        format!("{err}").contains("delivery is UNKNOWN"),
        "the display text must name the uncertainty: {err}"
    );
    let unrelated = Error::new(
        ErrorKind::Provider {
            code: "something_else".into(),
            retryable: true,
        },
        "not delivery unknown",
    );
    assert!(
        !is_delivery_unknown(&unrelated),
        "a different provider code must not classify as delivery-unknown"
    );
    let timeout = Error::timeout("plain timeout before any write");
    assert!(
        !is_delivery_unknown(&timeout),
        "a clean pre-write timeout must not classify as delivery-unknown"
    );
}
