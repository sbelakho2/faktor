//! Bounded deterministic allocation probe (second-angle performance audit).
//!
//! This test binary contains exactly ONE `#[test]`, so the counting
//! [`GlobalAlloc`] observes only this test's own allocations (no parallel
//! test threads in this process). The probe is bounded (fixed 64-message
//! corpus, no clock, no rng) and measures the per-render serialization
//! churn the audit found: the historical `measure_history` allocated ONE
//! fresh `String` per message; the scratch-buffer path reuses one buffer
//! sized by the LARGEST message and must allocate a bounded handful of
//! times for the whole window while producing the byte-identical digest.
//!
//! `unsafe_code` is allowed ONLY here: a counting global allocator is the
//! measurement instrument itself (the library under test stays
//! `unsafe`-free).
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

/// The historical algorithm, verbatim: one fresh `String` per message.
fn reference_history_allocations(
    messages: &[faktor_provider::RequestMessage],
) -> (usize, [u8; 32]) {
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0usize;
    for m in messages {
        let json = serde_json::to_string(m).unwrap_or_default();
        bytes = bytes.saturating_add(json.len());
        hasher.update(json.as_bytes());
    }
    (bytes, *hasher.finalize().as_bytes())
}

#[test]
fn history_render_allocations_are_bounded_not_per_message() {
    use faktor_context::tokenizer::GenericTokenCounter;
    use faktor_context::wire_plan::{measure_history_segment, measure_message};
    use faktor_provider::{ContentPart, RequestMessage, Role};

    // Fixed corpus: 64 messages × ~256 bytes of content (bounded, no rng).
    let messages: Vec<RequestMessage> = (0..64u32)
        .map(|i| RequestMessage {
            role: if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            },
            content: vec![ContentPart::text(format!("turn {i}: {}", "x".repeat(240)))],
        })
        .collect();

    // Reference token accounting through the public per-message seam (the
    // SAME formula the renderer's history segment charges).
    let want_tokens: u64 = messages
        .iter()
        .map(|m| measure_message(&GenericTokenCounter, m).count)
        .sum();

    // Warm any one-time lazy state (none expected) with an unarmed pass.
    let _ = reference_history_allocations(&messages);
    let _ = measure_history_segment(&GenericTokenCounter, &messages);

    let (reference, reference_calls, reference_bytes) =
        measure(|| reference_history_allocations(&messages));
    let (segment, scratch_calls, scratch_bytes) =
        measure(|| measure_history_segment(&GenericTokenCounter, &messages));

    // The measured before/after evidence (deterministic for the fixed
    // corpus; printed so `--nocapture` runs record exact counters).
    println!(
        "PROBE measure_history: reference allocations={reference_calls} bytes={reference_bytes} \
         | scratch allocations={scratch_calls} bytes={scratch_bytes} \
         | bytes_hashed={} tokens={}",
        reference.0, segment.tokens
    );

    // Correctness: byte-identical digest and byte total to the historical
    // per-message-String algorithm; the token count is the renderer's own.
    assert_eq!(segment.bytes, reference.0, "byte total drifted");
    assert_eq!(segment.hash.bytes(), reference.1, "history digest drifted");
    assert_eq!(segment.tokens, want_tokens, "token accounting drifted");

    // Evidence: the historical path allocates at least one String per
    // message (64 calls); the scratch path is bounded by the growth of ONE
    // buffer sized by the largest message — strict order-of-magnitude
    // reduction, bounded by a constant for this fixed corpus.
    assert!(
        reference_calls >= messages.len() as u64,
        "reference must allocate per message: {reference_calls} calls"
    );
    assert!(
        scratch_calls <= 16,
        "scratch path must not allocate per message: {scratch_calls} calls"
    );
    assert!(
        scratch_calls * 4 <= reference_calls,
        "expected an order-of-magnitude allocation-count reduction: \
         scratch {scratch_calls} vs reference {reference_calls}"
    );
    // The reference materializes the whole window in fresh buffers; the
    // scratch path never holds more than its one reusable buffer.
    assert!(
        scratch_bytes <= reference_bytes,
        "scratch bytes {scratch_bytes} exceeded reference bytes {reference_bytes}"
    );

    // ---- planner: the removed deep clone of every candidate -------------
    //
    // The historical planner pooled candidates through `.cloned()` (one
    // deep clone of id + omission_keys per candidate); the fixed planner
    // MOVES the request's candidates and only the SELECTED subset is
    // cloned into the returned plan. The reference reproduces the removed
    // clone work verbatim; with a tight budget most candidates are
    // unselected, so the gap is the eliminated per-candidate clones.
    use faktor_context::planner::{plan_context, ContextPlanRequest};
    use faktor_context::selection::{CandidateKind, ContextCandidate};

    let candidates: Vec<ContextCandidate> = (0..256u32)
        .map(|i| ContextCandidate {
            id: format!("note-{i:04}"),
            kind: CandidateKind::FileNote,
            bytes: 30,
            estimate_tokens: 10,
            utility: 0.01,
            omission_keys: vec![format!("key-{i:04}汉字")],
            ..ContextCandidate::default()
        })
        .collect();
    let make_request = || ContextPlanRequest {
        messages: Vec::new(),
        rules: Vec::new(),
        index_evidence: candidates.clone(),
        tool_notes: Vec::new(),
        subagent_summaries: Vec::new(),
        token_budget: 100, // fits ~10 of 256 candidates
        min_utility: 0.0,
        mode: faktor_context::planner::PlannerMode::default(),
    };

    /// The historical clone work, verbatim (`.iter().cloned().collect()` is
    /// intentional here: it recreates the exact growth profile the removed
    /// planner clone had, not a `to_vec()` exact-capacity shortcut).
    #[allow(clippy::iter_cloned_collect)]
    fn old_clone_reference(request: &ContextPlanRequest) -> usize {
        let messages: Vec<ContextCandidate> = request.messages.iter().cloned().collect();
        let rules = request.rules.clone();
        let evidence: Vec<ContextCandidate> = request
            .index_evidence
            .iter()
            .chain(request.tool_notes.iter())
            .chain(request.subagent_summaries.iter())
            .cloned()
            .collect();
        messages.len() + rules.len() + evidence.len()
    }

    let clone_reference = make_request();
    let (cloned_total, clone_calls, clone_bytes) =
        measure(|| old_clone_reference(&clone_reference));
    let plan_request = make_request();
    let (plan, plan_calls, plan_bytes) = measure(|| plan_context(plan_request));

    println!(
        "PROBE planner: removed clone-reference clones={cloned_total} \
         allocations={clone_calls} bytes={clone_bytes} \
         | plan_context allocations={plan_calls} bytes={plan_bytes} \
         | selected={}",
        plan.selected.len()
    );
    assert!(
        cloned_total >= candidates.len(),
        "reference must clone every candidate"
    );
    // Per-candidate deep clones are gone: the planner performs fewer than a
    // tenth of the removed clone calls (the remaining allocations are the
    // pool move, the selected-subset clones and the plan vectors).
    assert!(
        plan_calls * 10 < clone_calls,
        "the planner must no longer deep-clone the pool per candidate: \
         plan {plan_calls} vs removed clone calls {clone_calls}"
    );
    assert!(
        !plan.selected.is_empty(),
        "the tight budget still selects a window"
    );
}
