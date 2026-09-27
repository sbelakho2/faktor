//! `runtime::provider_loop_tests`: out-of-line tests.

#![allow(unused_imports)]

use super::*;
use crate::runtime::fixtures_tests::*;
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
