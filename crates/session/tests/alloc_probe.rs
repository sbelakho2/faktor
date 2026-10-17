//! Bounded deterministic allocation probe (second-angle performance audit).
//!
//! Exactly ONE `#[test]` in this binary: the counting [`GlobalAlloc`]
//! observes only this test's allocations (no parallel test threads in this
//! process). Proves the journal-append fix with counters: the historical
//! `decode_payload` (whose typed result the append path dropped) cloned the
//! entire payload `Value` into `TypedEventPayload::Opaque`; the new
//! `validate_payload` performs the SAME version/schema checks with ZERO
//! allocations on the opaque path — and both agree (accept/reject) on an
//! adversarial corpus.
//!
//! `unsafe_code` is allowed ONLY here: the counting global allocator is the
//! measurement instrument itself.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct CountingAllocator;

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: the probe's measurement instrument forwards every allocation to
// the system allocator unchanged; it owns no memory and has no invariants.
unsafe impl GlobalAlloc for CountingAllocator {
    // SAFETY: forwards a valid layout to `System`; the returned pointer is
    // exactly the system allocation (counter bookkeeping only).
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: `layout` is the caller's valid layout, forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    // SAFETY: `ptr` came from this allocator's `alloc`; forwarding is sound.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` came from `System.alloc`, forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    // SAFETY: `ptr` came from this allocator's `alloc`; forwarding is sound.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        // SAFETY: `ptr`/`layout` came from `System.alloc`; the new size is
        // forwarded unchanged to the same allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measure<T>(f: impl FnOnce() -> T) -> (T, u64, u64) {
    ALLOC_CALLS.store(0, Ordering::SeqCst);
    ALLOC_BYTES.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
    let out = f();
    ARMED.store(false, Ordering::SeqCst);
    (
        out,
        ALLOC_CALLS.load(Ordering::SeqCst),
        ALLOC_BYTES.load(Ordering::SeqCst),
    )
}

#[test]
fn journal_append_validation_allocates_zero_on_opaque_payloads_and_matches_decode() {
    use faktor_core::event::EventKind;
    use faktor_session::payload::TypedEventPayload;
    use faktor_session::{decode_payload, validate_payload, PAYLOAD_SCHEMA_V};

    // A large opaque payload (the append path's common case: an event row
    // whose kind has no registered typed schema).
    let big = serde_json::json!({
        "summary": "x".repeat(1 << 20),
        "rows": (0..64).map(|i| serde_json::json!({"i": i})).collect::<Vec<_>>(),
    });
    let kind = EventKind::ContextCompacted;

    let (decode_result, decode_calls, decode_bytes) =
        measure(|| decode_payload(kind, PAYLOAD_SCHEMA_V, Some(&big)));
    let (validate_result, validate_calls, validate_bytes) =
        measure(|| validate_payload(kind, PAYLOAD_SCHEMA_V, Some(&big)));

    println!(
        "PROBE journal append: decode allocations={decode_calls} bytes={decode_bytes} \
         | validate allocations={validate_calls} bytes={validate_bytes} \
         | payload_bytes~{}",
        big["summary"].as_str().map(str::len).unwrap_or(0)
    );

    // Correctness: both accept; decode produced the Opaque carrier.
    assert!(
        validate_result.is_ok(),
        "validate must accept a valid payload"
    );
    assert!(
        matches!(decode_result, Ok(TypedEventPayload::Opaque(Some(_)))),
        "decode must carry the opaque payload"
    );
    // Evidence: the historical path deep-clones the whole Value (≥ 1 MiB);
    // the validation-only path performs NO allocation at all.
    assert!(
        decode_bytes >= 1 << 20,
        "historical decode must clone the payload: {decode_bytes} bytes"
    );
    assert_eq!(
        (validate_calls, validate_bytes),
        (0, 0),
        "validate_payload must not allocate on the opaque path"
    );

    // Differential corpus: accept/reject parity on adversarial shapes
    // (empty, unicode, nested, missing payload, wrong version, hostile
    // Failed shapes).
    let hostile_nested = serde_json::json!({
        "a": [serde_json::json!({"汉字😀": {"deep": [[null, true, 1.5]]}})],
    });
    let cases: Vec<(EventKind, i64, Option<serde_json::Value>)> = vec![
        (EventKind::ContextCompacted, PAYLOAD_SCHEMA_V, None),
        (
            EventKind::ContextCompacted,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::Value::Null),
        ),
        (
            EventKind::ContextCompacted,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::json!({})),
        ),
        (
            EventKind::ModelChunkReceived,
            PAYLOAD_SCHEMA_V,
            Some(hostile_nested.clone()),
        ),
        (EventKind::TurnCompleted, PAYLOAD_SCHEMA_V, None),
        (EventKind::TurnCompleted, PAYLOAD_SCHEMA_V + 1, None),
        (
            EventKind::TurnCompleted,
            PAYLOAD_SCHEMA_V + 999,
            Some(serde_json::json!({"future": true})),
        ),
        (
            EventKind::Failed,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::json!({"message": "boom 汉字😀"})),
        ),
        (
            EventKind::Failed,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::json!({"error": "aborted", "op_id": 7})),
        ),
        (
            EventKind::Failed,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::json!({"error": "aborted"})),
        ),
        (
            EventKind::Failed,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::json!({"message": 42})),
        ),
        (
            EventKind::Failed,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::json!({})),
        ),
        (
            EventKind::Failed,
            PAYLOAD_SCHEMA_V,
            Some(serde_json::Value::Null),
        ),
        (EventKind::Failed, PAYLOAD_SCHEMA_V, None),
        (
            EventKind::Failed,
            PAYLOAD_SCHEMA_V + 1,
            Some(serde_json::json!({"message": "boom"})),
        ),
    ];
    for (i, (kind, ver, payload)) in cases.iter().enumerate() {
        let decoded = decode_payload(*kind, *ver, payload.as_ref());
        let validated = validate_payload(*kind, *ver, payload.as_ref());
        assert_eq!(
            decoded.is_ok(),
            validated.is_ok(),
            "row {i}: validate/decode accept-reject parity broke for {kind:?} v{ver} {payload:?}"
        );
        if let Err(err) = &validated {
            assert!(
                err.to_string().contains("schema")
                    || err.to_string().contains("violates")
                    || err.to_string().contains("unknown"),
                "row {i}: refusal must be typed and named: {err}"
            );
        }
    }

    // Failed-payload probe: the historical decode cloned the WHOLE Value
    // before parsing (including fields its schema ignores); the fixed decode
    // AND validate parse both shapes in place — strictly fewer bytes on a
    // payload carrying a large unparsed field.
    let failed = serde_json::json!({
        "error": "aborted",
        "op_id": 42,
        "unparsed_cargo": "x".repeat(1 << 20),
    });
    let (old_ok, old_calls, old_bytes) = measure(|| {
        serde_json::from_value::<faktor_session::payload::FailedAbortPayload>(failed.clone())
            .is_ok()
    });
    let (_, failed_decode_calls, failed_decode_bytes) =
        measure(|| decode_payload(EventKind::Failed, PAYLOAD_SCHEMA_V, Some(&failed)));
    let (_, failed_validate_calls, failed_validate_bytes) =
        measure(|| validate_payload(EventKind::Failed, PAYLOAD_SCHEMA_V, Some(&failed)));
    println!(
        "PROBE failed payload: old clone-then-parse calls={old_calls} bytes={old_bytes} \
         | decode calls={failed_decode_calls} bytes={failed_decode_bytes} \
         | validate calls={failed_validate_calls} bytes={failed_validate_bytes}"
    );
    assert!(old_ok, "reference parse must accept the abort shape");
    assert!(
        old_bytes >= 1 << 20,
        "the historical clone-then-parse must materialize the payload: {old_bytes} bytes"
    );
    assert!(
        failed_decode_bytes < old_bytes && failed_validate_bytes < old_bytes,
        "both fixed paths must skip the Value clone: old {old_bytes}, \
         decode {failed_decode_bytes}, validate {failed_validate_bytes}"
    );
    assert_eq!(
        (failed_decode_calls, failed_decode_bytes),
        (failed_validate_calls, failed_validate_bytes),
        "decode and validate must have the SAME allocation shape on Failed (both in place)"
    );
}
