// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Adversarial coverage of the live-output subscription: the real native
//! listener, a REAL `ChunkSink`→`drain_chunk_stream`→hub pipeline and
//! wire-level SSE assertions. Expected texts/offsets are fixture-owned
//! (the test's own strings), never derived from a served observation.
//!
//! Invariants under test:
//! * live text advances after a few bytes — long before the 8 KiB durable
//!   flush or completion;
//! * the handshake snapshot is an exact watermark (subscribe-before-submit
//!   can't lose the fast reply);
//! * the per-subscriber queue is bounded and lag is an EXPLICIT resync
//!   signal; the authoritative snapshot reconciles byte-identically;
//! * a reconnect during a subscriber gap never re-runs anything and
//!   yields the byte-identical transcript;
//! * cancellation keeps partial text with an accurate durable outcome;
//! * a restart changes the server-instance id (no cross-instance reuse);
//! * two simultaneous conversations never cross streams;
//! * UTF-8 byte offsets (never UTF-16 indices) with contiguous revisions;
//! * authenticated typed refusals before any effect.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;
use std::time::Duration;

use faktor_core::id::{OpId, SessionId};
use faktor_session::SessionHandle;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::api::tests::test_deps;
use crate::api::{serve_arc, ServerDeps, ServerHandle};
use crate::drain_chunk_stream;

/// Bounded wait for one SSE frame (a hung stream must fail, not hang).
const FRAME_TIMEOUT: Duration = Duration::from_secs(3);

struct LiveRig {
    dir: tempfile::TempDir,
    deps: Arc<ServerDeps>,
    sink: Arc<faktor_agent::ChunkSink>,
    _handle: ServerHandle,
    base: String,
    session: SessionId,
}

impl LiveRig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let (sink, rx) = faktor_agent::ChunkSink::channel();
        deps.chunk_rx = Some(rx);
        let ws = deps
            .session
            .create_workspace(dir.path().to_str().unwrap())
            .unwrap();
        let session = deps
            .session
            .create_session(ws, "live-output", "fake", "m")
            .unwrap();
        // The production drainer: chunk receiver → the live-output hub.
        drain_chunk_stream(&mut deps);
        let deps = Arc::new(deps);
        let handle = serve_arc(deps.clone(), 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        Self {
            dir,
            deps,
            sink,
            _handle: handle,
            base,
            session: session.id(),
        }
    }

    fn handle(&self) -> SessionHandle {
        self.deps
            .session
            .get_session(self.session)
            .unwrap()
            .unwrap()
    }

    /// Push one chunk through the REAL sink (the same producer the agent
    /// streams to) into THIS rig's session.
    fn publish(&self, mid: Option<i64>, kind: &'static str, text: &str) {
        self.publish_in(self.session, mid, kind, text);
    }

    /// Push one chunk addressed to an explicit session (cross-session
    /// isolation is asserted at the subscriber, not at the publisher).
    fn publish_in(&self, session: SessionId, mid: Option<i64>, kind: &'static str, text: &str) {
        self.sink.try_send(faktor_agent::ChunkEvent {
            session_id: session,
            message_id: mid,
            kind,
            text: text.to_string(),
        });
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .get(format!("{}{}", self.base, path))
            .bearer_auth(self.deps.auth_token.as_str())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
        (status, value)
    }

    /// GET without credentials (for auth-refusal cases).
    async fn get_raw(&self, path: &str) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}));
        (status, value)
    }

    async fn open_sse(&self, path: &str) -> Sse {
        let response = reqwest::Client::new()
            .get(format!("{}{}", self.base, path))
            .bearer_auth(self.deps.auth_token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "live/output must answer 200"
        );
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            content_type.starts_with("text/event-stream"),
            "SSE content type: {content_type}"
        );
        Sse {
            response,
            buf: String::new(),
        }
    }

    /// One assistant message row to stream into (durably created, like the
    /// agent's `ensure_assistant_message`).
    async fn assistant_message(&self, handle: &SessionHandle) -> i64 {
        handle
            .append_message(1, "user", json!({"text": "prompt"}))
            .await
            .unwrap();
        handle
            .append_message(2, "assistant", json!({}))
            .await
            .unwrap()
    }
}

/// Minimal SSE reader over the real HTTP response body.
struct Sse {
    response: reqwest::Response,
    buf: String,
}

impl Sse {
    async fn frame(&mut self) -> Option<(String, Value)> {
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let raw: String = self.buf.drain(..end + 2).collect();
                let mut event = String::new();
                let mut data = String::new();
                for line in raw.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        event = rest.to_string();
                    } else if let Some(rest) = line.strip_prefix("data: ") {
                        data = rest.to_string();
                    }
                }
                if event.is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(&data)
                    .unwrap_or_else(|_| json!({"raw": data, "event": event}));
                return Some((event, value));
            }
            match timeout(FRAME_TIMEOUT, self.response.chunk()).await {
                Ok(Ok(Some(chunk))) => self.buf.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(Ok(None)) => return None,
                Ok(Err(_)) => return None,
                Err(_) => panic!("live/output SSE frame timeout (buf={})", self.buf.len()),
            }
        }
    }

    async fn expect(&mut self, event: &str) -> Value {
        let (name, value) = self
            .frame()
            .await
            .unwrap_or_else(|| panic!("stream ended before an `{event}` frame"));
        assert_eq!(name, event, "expected `{event}` frame, got {name}: {value}");
        value
    }
}

#[tokio::test]
async fn live_chunks_advance_long_before_any_durable_flush() {
    let rig = LiveRig::new().await;
    let handle = rig.handle();
    let mid = rig.assistant_message(&handle).await;

    // Subscribe BEFORE publishing: the handshake must land first.
    let mut sse = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    let handshake = sse.expect("snapshot").await;
    assert_eq!(handshake["session"], json!(rig.session.raw()));
    assert_eq!(handshake["turn"], Value::Null);
    assert_eq!(
        handshake["instance"].as_str().unwrap().len(),
        16,
        "the handshake carries the server-instance id"
    );
    let start = handshake["sequence"].as_u64().unwrap();

    // Fixture: a tiny reply, far below the 8 KiB durable-flush bound.
    let chunks = ["Hel", "lo ", "w", "orld"];
    let mut expect_offset = 0u64;
    let mut seen = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        rig.publish(Some(mid), "text", chunk);
        let frame = sse.expect("live").await;
        assert_eq!(frame["messageId"], json!(mid));
        assert_eq!(frame["kind"], json!("text"));
        assert_eq!(frame["partId"], json!(format!("{mid}:text")));
        assert_eq!(frame["sequence"], json!(start + 1 + i as u64));
        assert_eq!(frame["revision"], json!(i as u64 + 1));
        assert_eq!(frame["offset"], json!(expect_offset));
        assert_eq!(frame["content"], json!(chunk));
        expect_offset += chunk.len() as u64;
        seen.push_str(chunk);
    }
    // THE fluidity invariant: visible text advanced after 12 bytes with
    // ZERO durable part rows (no 8 KiB flush, no completion).
    let rows = handle.parts_of(mid).unwrap();
    assert!(
        rows.is_empty(),
        "no per-token durable write happened: {} rows",
        rows.len()
    );
    assert_eq!(seen, "Hello world", "fixture text, received in order");

    // The authoritative snapshot agrees byte-for-byte and reports the
    // durable floor honestly.
    let (status, snapshot) = rig
        .get(&format!(
            "/native/session/{}/live/snapshot?message={mid}&kind=text",
            rig.session.raw()
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let part = &snapshot["parts"][0];
    assert_eq!(part["text"], json!("Hello world"));
    assert_eq!(part["offset"], json!(11));
    assert_eq!(part["durableBytes"], json!(0));
    assert_eq!(part["liveTail"], json!(true));
}

#[tokio::test]
async fn subscribe_before_submit_snapshot_is_the_exact_watermark() {
    let rig = LiveRig::new().await;
    let handle = rig.handle();
    let mid = rig.assistant_message(&handle).await;

    let mut sse = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    let handshake = sse.expect("snapshot").await;
    let watermark = handshake["sequence"].as_u64().unwrap();

    rig.publish(Some(mid), "text", "fast");
    let frame = sse.expect("live").await;
    assert_eq!(
        frame["sequence"].as_u64().unwrap(),
        watermark + 1,
        "the first reply frame is exactly watermark+1: nothing outran the listener"
    );
    assert_eq!(frame["offset"], json!(0));

    rig.publish(Some(mid), "text", " reply");
    let frame = sse.expect("live").await;
    assert_eq!(frame["sequence"].as_u64().unwrap(), watermark + 2);
    assert_eq!(frame["offset"], json!(4));
    assert_eq!(frame["content"], json!(" reply"));
}

#[tokio::test]
async fn reconnect_after_a_subscriber_gap_is_byte_identical_and_never_replays() {
    let rig = LiveRig::new().await;
    let handle = rig.handle();
    let mid = rig.assistant_message(&handle).await;

    // First connection: receive a prefix, then the subscriber goes away
    // (a crash/restart of the IDE) while the reply keeps streaming.
    {
        let mut sse = rig
            .open_sse(&format!(
                "/native/session/{}/live/output",
                rig.session.raw()
            ))
            .await;
        sse.expect("snapshot").await;
        rig.publish(Some(mid), "text", "partial ");
        let frame = sse.expect("live").await;
        assert_eq!(frame["content"], json!("partial "));
        // Sse dropped here = the subscriber gap.
    }

    rig.publish(Some(mid), "text", "middle ");
    rig.publish(Some(mid), "text", "tail");

    // Reconnect: the handshake snapshot IS the reconciliation — full text,
    // no frame replay, no shell command re-runs (nothing executes here).
    let mut sse = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    let handshake = sse.expect("snapshot").await;
    let part = &handshake["parts"][0];
    assert_eq!(
        part["text"],
        json!("partial middle tail"),
        "the snapshot carries the byte-identical text across the gap"
    );
    assert_eq!(part["messageId"], json!(mid));

    // And the stream CONTINUES from exactly the snapshot's offset: the
    // next delta appends, never duplicates.
    rig.publish(Some(mid), "text", "!");
    let frame = sse.expect("live").await;
    assert_eq!(
        frame["offset"].as_u64().unwrap(),
        part["offset"].as_u64().unwrap()
    );
    assert_eq!(frame["content"], json!("!"));
}

#[tokio::test]
async fn completion_lands_in_the_turn_projection_and_a_later_prompt_still_works() {
    let rig = LiveRig::new().await;
    let handle = rig.handle();
    let mid = rig.assistant_message(&handle).await;
    let op = OpId::try_from(9_001u64).unwrap();
    handle
        .start_turn_record(op, None, None, "fake", "m", None)
        .unwrap();

    let mut sse = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    let handshake = sse.expect("snapshot").await;
    assert_eq!(handshake["turn"]["status"], json!("active"));
    assert_eq!(handshake["turn"]["op"], json!("9001"));

    rig.publish(Some(mid), "text", "done text");
    let frame = sse.expect("live").await;
    assert_eq!(frame["turn"], json!("9001"));

    // Chunks drying up is NOT completion: no turn frame may arrive from a
    // quiet stream alone. The durable turn record is the authority.
    assert!(
        handle.finish_turn_record(op, "completed").unwrap(),
        "the fixture turn was active"
    );
    let turn = sse.expect("turn").await;
    assert_eq!(turn["turn"], json!("9001"));
    assert_eq!(turn["status"], json!("completed"));

    // A later prompt still works: a NEW turn + message streams into its
    // OWN part lineage (fresh offsets, fresh revisions, same instance).
    // Production admission journals an event that wakes every subscriber's
    // turn projection; the fixture forces the same wake explicitly.
    let op2 = OpId::try_from(9_002u64).unwrap();
    handle
        .start_turn_record(op2, None, None, "fake", "m", None)
        .unwrap();
    handle
        .force_append_event(
            faktor_core::event::EventKind::PhaseChanged,
            faktor_core::state::AgentState::WaitingForModel,
            Some(op2),
            None,
        )
        .unwrap();
    let turn = sse.expect("turn").await;
    assert_eq!(turn["turn"], json!("9002"));
    assert_eq!(turn["status"], json!("active"));

    let mid2 = handle
        .append_message(3, "assistant", json!({}))
        .await
        .unwrap();
    rig.publish(Some(mid2), "text", "second turn");
    let frame = sse.expect("live").await;
    assert_eq!(frame["messageId"], json!(mid2));
    assert_eq!(frame["offset"], json!(0));
    assert_eq!(frame["turn"], json!("9002"));
    assert!(handle.finish_turn_record(op2, "completed").unwrap());
    let turn = sse.expect("turn").await;
    assert_eq!(turn["turn"], json!("9002"));
    assert_eq!(turn["status"], json!("completed"));
}

#[tokio::test]
async fn cancellation_keeps_partial_text_and_reports_the_durable_outcome() {
    let rig = LiveRig::new().await;
    let handle = rig.handle();
    let mid = rig.assistant_message(&handle).await;
    let op = OpId::try_from(77u64).unwrap();
    handle
        .start_turn_record(op, None, None, "fake", "m", None)
        .unwrap();

    // Subscribe and read both frames first: the hub's live tail is the
    // synchronization point for everything the snapshot asserts below.
    let mut sse = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    sse.expect("snapshot").await;
    rig.publish(Some(mid), "text", "partial ");
    let frame = sse.expect("live").await;
    assert_eq!(frame["content"], json!("partial "));
    rig.publish(Some(mid), "text", "reply");
    let frame = sse.expect("live").await;
    assert_eq!(frame["content"], json!("reply"));
    assert_eq!(frame["turn"], json!("77"));

    // The runtime closes the record on cancellation (the same durable
    // mutation `POST /abort` drives for a tracked op).
    handle.finish_turn_record(op, "cancelled").unwrap();

    // Partial text survives cancellation byte-for-byte, and the outcome
    // label comes from the durable turn record — never from the stream
    // going quiet.
    let (status, snapshot) = rig
        .get(&format!(
            "/native/session/{}/live/snapshot?message={mid}&kind=text",
            rig.session.raw()
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["parts"][0]["text"], json!("partial reply"));
    assert_eq!(snapshot["parts"][0]["offset"], json!(13));
    assert_eq!(snapshot["turn"]["status"], json!("cancelled"));
    assert_eq!(snapshot["turn"]["op"], json!("77"));

    // The subscription announces the SAME durable outcome as a `turn`
    // frame (durable authority, bounded poll after the record closed).
    let turn = sse.expect("turn").await;
    assert_eq!(turn["turn"], json!("77"));
    assert_eq!(turn["status"], json!("cancelled"));
}

#[tokio::test]
async fn two_simultaneous_conversations_never_cross_streams() {
    let rig = LiveRig::new().await;
    let ws = rig
        .deps
        .session
        .create_workspace(rig.dir.path().to_str().unwrap())
        .unwrap();
    let other = rig
        .deps
        .session
        .create_session(ws, "other", "fake", "m")
        .unwrap();

    let handle_a = rig.handle();
    let handle_b = rig.deps.session.get_session(other.id()).unwrap().unwrap();
    let mid_a = rig.assistant_message(&handle_a).await;
    handle_b
        .append_message(1, "assistant", json!({}))
        .await
        .unwrap();
    let mid_b = handle_b
        .append_message(2, "assistant", json!({}))
        .await
        .unwrap();

    let mut sse_a = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    let mut sse_b = rig
        .open_sse(&format!("/native/session/{}/live/output", other.id().raw()))
        .await;
    sse_a.expect("snapshot").await;
    sse_b.expect("snapshot").await;

    // Interleave publishers on ONE hub; each subscriber must see only its
    // own session's content, in its own order.
    rig.publish(Some(mid_a), "text", "AAA-1 ");
    rig.publish_in(other.id(), Some(mid_b), "text", "BBB-1 ");
    rig.publish(Some(mid_a), "text", "AAA-2");
    rig.publish_in(other.id(), Some(mid_b), "text", "BBB-2");

    let mut got_a = String::new();
    for _ in 0..2 {
        let frame = sse_a.expect("live").await;
        assert_eq!(frame["session"], json!(rig.session.raw()));
        assert_eq!(frame["messageId"], json!(mid_a));
        got_a.push_str(frame["content"].as_str().unwrap());
    }
    let mut got_b = String::new();
    for _ in 0..2 {
        let frame = sse_b.expect("live").await;
        assert_eq!(frame["session"], json!(other.id().raw()));
        assert_eq!(frame["messageId"], json!(mid_b));
        got_b.push_str(frame["content"].as_str().unwrap());
    }
    assert_eq!(got_a, "AAA-1 AAA-2");
    assert_eq!(got_b, "BBB-1 BBB-2");
}

#[tokio::test]
async fn utf8_offsets_count_bytes_and_frames_stay_contiguous() {
    let rig = LiveRig::new().await;
    let handle = rig.handle();
    let mid = rig.assistant_message(&handle).await;

    let mut sse = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    sse.expect("snapshot").await;

    // Fixture byte lengths: "é" = 2, "世" = 3, "界" = 3 (UTF-8 BYTES —
    // the wire offsets are byte offsets, never UTF-16 code units).
    let chunks = ["é", "世", "界"];
    let expected_offsets = [0u64, 2, 5];
    for (chunk, expected_offset) in chunks.iter().zip(expected_offsets) {
        rig.publish(Some(mid), "text", chunk);
        let frame = sse.expect("live").await;
        assert_eq!(
            frame["offset"],
            json!(expected_offset),
            "byte offset of {chunk:?}"
        );
    }
    assert_eq!(expected_offsets[2] + 3, 8, "fixture: 8 bytes total");

    // Reasoning is its own logical part: independent offsets/revisions.
    rig.publish(Some(mid), "reasoning", "think");
    let frame = sse.expect("live").await;
    assert_eq!(frame["kind"], json!("reasoning"));
    assert_eq!(frame["offset"], json!(0));
    assert_eq!(frame["revision"], json!(1));

    // Tool payloads are discrete (not an accumulating text part): each is
    // its own frame with offset 0 and is never concatenated.
    rig.publish(Some(mid), "tool", "{\"name\":\"ls\"}");
    let frame = sse.expect("live").await;
    assert_eq!(frame["kind"], json!("tool"));
    assert_eq!(frame["offset"], json!(0));
}

#[tokio::test]
async fn durable_prefix_and_live_tail_reconcile_without_duplication() {
    let rig = LiveRig::new().await;
    let handle = rig.handle();
    let mid = rig.assistant_message(&handle).await;

    let mut sse = rig
        .open_sse(&format!(
            "/native/session/{}/live/output",
            rig.session.raw()
        ))
        .await;
    sse.expect("snapshot").await;

    // The first four bytes stream live AND are the bytes a durable 8 KiB
    // flush already landed (fixture: "ABCD") — the flush never lags past
    // what the hub received, exactly like production `text_buf` flushes.
    rig.publish(Some(mid), "text", "ABCD");
    let frame = sse.expect("live").await;
    assert_eq!(frame["offset"], json!(0));
    handle.append_text_part(mid, "ABCD").await.unwrap();
    rig.publish(Some(mid), "text", "EFGH");
    let frame = sse.expect("live").await;
    assert_eq!(frame["offset"], json!(4));

    let (status, snapshot) = rig
        .get(&format!(
            "/native/session/{}/live/snapshot?message={mid}&kind=text",
            rig.session.raw()
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let part = &snapshot["parts"][0];
    assert_eq!(part["text"], json!("ABCDEFGH"));
    assert_eq!(part["offset"], json!(8));
    assert_eq!(part["durableBytes"], json!(4));
    assert!(part["durableRevision"].as_i64().unwrap() > 0);
    assert_eq!(part["liveTail"], json!(true));

    // The next live frame continues at the snapshot offset EXACTLY: a
    // client that adopted the snapshot appends once (no duplicate).
    rig.publish(Some(mid), "text", "IJKL");
    let frame = sse.expect("live").await;
    assert_eq!(frame["offset"], json!(8));
    assert_eq!(frame["content"], json!("IJKL"));
}

#[tokio::test]
async fn a_restart_changes_the_server_instance_id() {
    // Two daemon constructions (a restart) must never share an instance
    // id: sequence spaces are per-instance and a client that sees a new id
    // resyncs instead of trusting the old cursor.
    let rig = LiveRig::new().await;
    let handshake = {
        let mut sse = rig
            .open_sse(&format!(
                "/native/session/{}/live/output",
                rig.session.raw()
            ))
            .await;
        sse.expect("snapshot").await
    };
    let first = handshake["instance"].as_str().unwrap().to_string();
    assert_eq!(first.len(), 16, "instance ids are fixed-width random hex");

    let second_rig = LiveRig::new().await;
    let second = second_rig.deps.live_output.instance().to_string();
    assert_eq!(second.len(), 16);
    assert_ne!(
        first, second,
        "a restart changes the server-instance id (differential property)"
    );

    // The served snapshot and the hub it comes from agree on identity.
    assert_eq!(
        handshake["instance"],
        json!(rig.deps.live_output.instance())
    );
}

#[tokio::test]
async fn live_routes_refuse_unauthenticated_and_hostile_queries_typed() {
    let rig = LiveRig::new().await;
    let sid = rig.session.raw();

    // No credentials: 401 before any resolve/effect.
    let (status, body) = rig
        .get_raw(&format!("/native/session/{sid}/live/output"))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, body) = rig
        .get_raw(&format!("/native/session/{sid}/live/snapshot"))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    // Unknown session: 404. Hostile id: 400.
    let (status, body) = rig.get("/native/session/999999/live/snapshot").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = rig.get("/native/session/-1/live/snapshot").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Strict DTOs: unknown query keys and half-specified filters are 400s.
    let (status, body) = rig
        .get(&format!("/native/session/{sid}/live/snapshot?bogus=1"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig
        .get(&format!("/native/session/{sid}/live/snapshot?message=1"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig
        .get(&format!(
            "/native/session/{sid}/live/snapshot?message=1&kind=tool_call"
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig
        .get(&format!("/native/session/{sid}/live/output?after=-4"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // POST on the SSE route: typed 405 (the frozen method fallback).
    let response = reqwest::Client::new()
        .post(format!("{}/native/session/{sid}/live/output", rig.base))
        .bearer_auth(rig.deps.auth_token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// ------------------------------------------------------------------- hub

#[tokio::test]
async fn hub_queue_is_bounded_and_lag_is_an_explicit_signal() {
    let hub = Arc::new(crate::native::live_output::LiveOutputHub::new());
    let sid = SessionId::new(4);
    let (mut rx, _, _) = hub.subscribe(sid);
    let sequence_zero = hub.sequence();
    assert_eq!(sequence_zero, 0, "fixture: a fresh hub starts at 0");

    // The receiver idles while MORE than the ring capacity is published:
    // the queue must not grow — the receiver is told exactly how many
    // frames it missed (broadcast ring contract: capacity 256 published
    // past = dropped count of the overrun).
    let total = crate::native::live_output::LIVE_BROADCAST_CAPACITY + 7;
    for i in 0..total {
        hub.publish(&faktor_agent::ChunkEvent {
            session_id: sid,
            message_id: Some(1),
            kind: "text",
            text: format!("c{i:04}"),
        });
    }
    match rx.recv().await {
        Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
            assert_eq!(dropped, 7, "exactly the frames beyond the ring");
        }
        other => panic!("lag must be explicit, got {other:?}"),
    }
    // After the resync signal the stream resumes at the ring head, in
    // order, with contiguous per-part revisions. Fixture: every delta is
    // 5 UTF-8 bytes ("c0000" style), so the retained head (frame index 7)
    // sits at byte offset 35.
    let first = rx.recv().await.unwrap();
    assert_eq!(first.offset, 35, "ring head offset (5 bytes per delta)");
    let second = rx.recv().await.unwrap();
    assert_eq!(second.offset, first.offset + 5);
    assert_eq!(second.revision, first.revision + 1);
}

#[tokio::test]
async fn hub_drops_unattachable_chunks_and_rebases_onto_the_durable_authority() {
    let hub = crate::native::live_output::LiveOutputHub::new();
    let sid = SessionId::new(5);

    // No message id / empty text: unattachable — the sequence must not
    // advance (frames without a transcript part are dropped, never sent).
    let before = hub.sequence();
    hub.publish(&faktor_agent::ChunkEvent {
        session_id: sid,
        message_id: None,
        kind: "text",
        text: "orphan".to_string(),
    });
    hub.publish(&faktor_agent::ChunkEvent {
        session_id: sid,
        message_id: Some(1),
        kind: "text",
        text: String::new(),
    });
    assert_eq!(hub.sequence(), before);

    // Live text that diverges from the durable authority (a mid-stream
    // gap) reconciles ONCE onto the durable text: composed = durable,
    // the lineage resets to its length and `attempt` bumps so every
    // subscriber resyncs exactly once (no resync loop).
    hub.publish(&faktor_agent::ChunkEvent {
        session_id: sid,
        message_id: Some(2),
        kind: "text",
        text: "XYZ".to_string(),
    });
    let durable = "AB";
    let (composed, consistent) =
        crate::native::live_output::compose_live_text(Some(("XYZ", 0)), durable);
    assert_eq!(
        composed, durable,
        "the durable authority wins on divergence"
    );
    assert!(!consistent);
    hub.rebase(sid, 2, "text", &composed);

    hub.publish(&faktor_agent::ChunkEvent {
        session_id: sid,
        message_id: Some(2),
        kind: "text",
        text: "C".to_string(),
    });
    let (_, _, _, _, attempt) = hub.live_text(sid, 2, "text").unwrap();
    assert_eq!(
        attempt, 1,
        "the rebase bumped the lineage epoch exactly once"
    );
    let (next_offset, _, _) = hub.part_lineage(sid, 2, "text").unwrap();
    assert_eq!(
        next_offset, 3,
        "the rebased lineage continues from the composed durable length (2) + 1"
    );
}

#[tokio::test]
async fn compose_covers_every_reconciliation_direction() {
    use crate::native::live_output::compose_live_text;

    // Live extends the durable prefix (the normal streaming case): the
    // live tail wins whole, marked consistent.
    let (text, consistent) = compose_live_text(Some(("ABCDEFGH", 0)), "ABCD");
    assert_eq!(text, "ABCDEFGH");
    assert!(consistent);

    // Durable ahead of a memory-dropped live cache: durable wins.
    let (text, consistent) = compose_live_text(Some(("AB", 0)), "ABCDEFGH");
    assert_eq!(text, "ABCDEFGH");
    assert!(!consistent);

    // Divergence at the first byte: durable wins (no mixed text).
    let (text, consistent) = compose_live_text(Some(("ZZ", 0)), "ABCD");
    assert_eq!(text, "ABCD");
    assert!(!consistent);

    // Base inside the durable range: durable prefix + live suffix beyond
    // the durable end, composed on exact byte boundaries.
    let (text, consistent) = compose_live_text(Some(("DEFGH", 3)), "ABCD");
    assert_eq!(text, "ABCDEFGH");
    assert!(consistent);

    // A live base BEYOND the durable end cannot be stitched: the durable
    // authority stands until its next flush crosses the gap.
    let (text, consistent) = compose_live_text(Some(("EFGH", 9)), "ABCD");
    assert_eq!(text, "ABCD");
    assert!(!consistent);

    // No live state at all: pure durable.
    let (text, consistent) = compose_live_text(None, "ABCD");
    assert_eq!(text, "ABCD");
    assert!(!consistent);
}
