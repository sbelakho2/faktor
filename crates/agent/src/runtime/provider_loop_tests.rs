//! `runtime::provider_loop_tests`: out-of-line tests.

use super::*;

use crate::runtime::tests::*;
use crate::*;

#[tokio::test]
async fn chunk_sink_delivers_every_frame_in_order_under_normal_rates() {
    let (sink, mut rx) = ChunkSink::channel();
    let sid = SessionId::new(1);
    for i in 0..5 {
        sink.try_send(text_event(sid, 1, &format!("t{i}")));
    }
    assert_eq!(sink.buffered_bytes(), 0, "healthy path never buffers");
    let mut seen = Vec::new();
    for _ in 0..5 {
        seen.push(rx.recv().await.expect("frame").text);
    }
    assert_eq!(sink.dropped_bytes(), 0, "no backpressure, nothing dropped");
    drop(sink);
    assert!(
        rx.recv().await.is_none(),
        "no extra frames after the sender is gone"
    );
    assert_eq!(seen, ["t0", "t1", "t2", "t3", "t4"]);
}

#[tokio::test]
async fn chunk_sink_same_key_deltas_coalesce_and_flush_when_room_returns() {
    // Deltas of the SAME (session, message, kind) merge into one frame
    // while full and flush in order as soon as the channel drains.
    let (sink, mut rx) = ChunkSink::channel();
    let sid = SessionId::new(1);
    for _ in 0..CHUNK_CHANNEL_CAPACITY {
        sink.try_send(text_event(sid, 1, "a"));
    }
    for _ in 0..2000 {
        sink.try_send(text_event(sid, 1, "b"));
    }
    assert!(
        sink.buffered_bytes() <= CHUNK_COALESCE_CAP_BYTES,
        "coalescer buffer exceeded its cap: {}",
        sink.buffered_bytes()
    );
    assert_eq!(
        sink.dropped_bytes(),
        0,
        "2000 x 1B fits the cap: nothing dropped"
    );
    // Drain the in-flight frames, then one more emit flushes the
    // coalesced frame FIRST (FIFO) as a single 2000-byte frame.
    for _ in 0..CHUNK_CHANNEL_CAPACITY {
        rx.recv().await.unwrap();
    }
    sink.try_send(text_event(sid, 1, "c"));
    let coalesced = rx.recv().await.unwrap();
    assert_eq!(coalesced.text, "b".repeat(2000));
    let last = rx.recv().await.unwrap();
    assert_eq!(last.text, "c");
}

#[tokio::test]
async fn chunk_sink_never_mixes_frames_across_sessions() {
    // Different streams while full never merge: a frame's text belongs
    // to exactly one (session, message). The older stream's buffered
    // frame may be REPLACED (drop-oldest) but never corrupted.
    let (sink, mut rx) = ChunkSink::channel();
    let sid_a = SessionId::new(1);
    let sid_b = SessionId::new(2);
    for _ in 0..CHUNK_CHANNEL_CAPACITY {
        sink.try_send(text_event(sid_a, 1, "a"));
    }
    // Channel full: A's delta goes pending, then B's delta replaces it
    // (keep-newest) instead of merging into A's frame.
    sink.try_send(text_event(sid_a, 1, "a"));
    sink.try_send(text_event(sid_b, 1, "b"));
    assert!(sink.buffered_bytes() > 0);
    // Free one slot: the next emit flushes B's pending frame first.
    rx.recv().await.unwrap();
    sink.try_send(text_event(sid_b, 1, "b"));
    drop(sink);
    let mut saw_b = false;
    while let Some(ev) = rx.recv().await {
        assert!(
            ev.session_id == sid_a || ev.session_id == sid_b,
            "unexpected session in frame"
        );
        if ev.session_id == sid_a {
            assert!(
                !ev.text.contains('b'),
                "A frame corrupted with B text: {ev:?}"
            );
        } else {
            saw_b = true;
            assert!(
                !ev.text.contains('a'),
                "B frame corrupted with A text: {ev:?}"
            );
        }
    }
    assert!(saw_b, "session B must still receive frames after recovery");
}

#[tokio::test]
async fn live_chunk_sink_delivers_text_deltas_under_normal_rates() {
    // Audit 41 regression: the bounded channel preserves streaming text
    // delivery under normal rates (the daemon-level integration flow
    // asserts the resulting session.next.text.delta SSE frame).
    let script = vec![
        ScriptedResponse::Text("hel".into()),
        ScriptedResponse::Text("lo".into()),
        ScriptedResponse::End,
    ];
    let (mut deps, _dir) = deps(scripted_provider(script), vec![]);
    let (sink, mut rx) = ChunkSink::channel();
    deps.chunk_sink = Some(sink);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    runtime.run_turn(session, "hi", &[]).await.unwrap();
    drop(runtime);
    let mut frames = Vec::new();
    while let Some(ev) = rx.recv().await {
        frames.push(ev);
    }
    let text: String = frames
        .iter()
        .filter(|f| f.kind == "text")
        .map(|f| f.text.as_str())
        .collect();
    assert_eq!(text, "hello", "bounded delivery must carry the stream text");
    assert!(
        frames.iter().all(|f| f.session_id == session),
        "all frames must carry the emitting session"
    );
}

#[tokio::test]
async fn live_chunk_turn_under_full_channel_completes_bounded() {
    // Audit 41 end-to-end: a turn whose model emits 5000 deltas into a
    // channel nobody drains must still complete (never blocked) and the
    // recoverable live text stays within the structural bound.
    let big: Vec<ScriptedResponse> = (0..5000)
        .map(|_| ScriptedResponse::Text("y".repeat(100)))
        .chain(std::iter::once(ScriptedResponse::End))
        .collect();
    let (mut deps, _dir) = deps(scripted_provider(big), vec![]);
    let (sink, mut rx) = ChunkSink::channel();
    deps.chunk_sink = Some(sink);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let t0 = std::time::Instant::now();
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(20),
        "the turn must complete in bounded time with a full chunk channel"
    );
    drop(runtime);
    let mut received = 0u64;
    while let Some(ev) = rx.recv().await {
        received += ev.text.len() as u64;
    }
    assert!(
        received <= CHUNK_CHANNEL_CAPACITY as u64 * 100 + CHUNK_COALESCE_CAP_BYTES as u64 + 4096,
        "recoverable live text must be structurally bounded, got {received}"
    );
}

/// Finding: a closed receiver must not swallow the current event's bytes
/// from the operational drop ledger.
#[tokio::test]
async fn chunk_sink_closed_receiver_counts_current_event() {
    let (sink, rx) = ChunkSink::channel();
    let sid = SessionId::new(7);
    drop(rx);
    sink.try_send(text_event(sid, 1, "abc"));
    assert_eq!(sink.dropped_bytes(), 3, "the closed event must be counted");
    assert_eq!(sink.buffered_bytes(), 0);
}

/// Finding: a close during the FIFO flush loses BOTH the buffered frame and
/// the incoming event; both must be counted.
#[tokio::test]
async fn chunk_sink_closed_receiver_counts_pending_plus_current() {
    let (sink, rx) = ChunkSink::channel();
    let sid = SessionId::new(8);
    // Fill the bounded channel with distinct keys (no coalescing), then one
    // more event lands in the pending frame.
    for i in 0..CHUNK_CHANNEL_CAPACITY {
        sink.try_send(text_event(sid, i as i64 + 1, "z"));
    }
    sink.try_send(text_event(sid, 999_999, "pq"));
    assert_eq!(sink.buffered_bytes(), 2, "the overflow frame is pending");
    drop(rx);
    sink.try_send(text_event(sid, 1_000_000, "xyz"));
    assert_eq!(
        sink.dropped_bytes(),
        5,
        "pending (2) + current (3) must both be counted"
    );
    assert_eq!(sink.buffered_bytes(), 0);
}

/// Finding: once the sender is closed, EVERY future delta is accounted.
#[tokio::test]
async fn chunk_sink_after_close_counts_every_future_delta() {
    let (sink, rx) = ChunkSink::channel();
    let sid = SessionId::new(9);
    drop(rx);
    let mut produced = 0u64;
    for i in 0..25 {
        let text = "q".repeat(i % 7 + 1);
        produced += text.len() as u64;
        sink.try_send(text_event(sid, i as i64 + 1, &text));
    }
    assert_eq!(sink.dropped_bytes(), produced);
    assert_eq!(sink.buffered_bytes(), 0);
}

/// Finding: the sink's drop ledger must be exact at every step —
/// produced == delivered + buffered + dropped — including through
/// coalescing, replacements and close.
#[tokio::test]
async fn chunk_sink_drop_accounting_equals_produced_minus_delivered_minus_buffered() {
    let (sink, mut rx) = ChunkSink::channel();
    let sid = SessionId::new(10);
    let mut produced: u64 = 0;
    let mut delivered: u64 = 0;
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    for step in 0..500u32 {
        let len = (next() % 40) as usize;
        let text = "x".repeat(len);
        produced += len as u64;
        sink.try_send(text_event(sid, (next() % 3) as i64 + 1, &text));
        // Drain the bounded channel so "not delivered" is entirely the
        // sink's own pending frame or its drop ledger at the assertion.
        while let Ok(ev) = rx.try_recv() {
            delivered += ev.text.len() as u64;
        }
        assert_eq!(
            produced,
            delivered + sink.buffered_bytes() as u64 + sink.dropped_bytes(),
            "accounting ledger must balance at step {step}"
        );
    }
    while let Ok(ev) = rx.try_recv() {
        delivered += ev.text.len() as u64;
    }
    assert_eq!(
        produced,
        delivered + sink.buffered_bytes() as u64 + sink.dropped_bytes()
    );
    drop(rx);
    for i in 0..50 {
        let text = "y".repeat(3);
        produced += 3;
        sink.try_send(text_event(sid, i as i64 + 1, &text));
    }
    assert_eq!(
        produced,
        delivered + sink.buffered_bytes() as u64 + sink.dropped_bytes(),
        "close losses must keep the ledger balanced"
    );
}
