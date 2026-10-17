//! Manual loopback streaming-delivery harness (never part of the standing
//! suites): one real daemon listener and one real TCP/SSE client.
//!
//! Scenarios:
//! - 200 deltas/s x 10_000 durable deltas (frame rate, wire bytes,
//!   event->client latency percentiles),
//! - 10_000-event catch-up (delivery rate and order),
//! - 1_000 non-delta state events in one burst (immediate-flush latency),
//! - 10_000 terminal lifetime rows walked with a keyset cursor (paging cost).
//!
//! Feature-gated (never part of the standing suites): run explicitly with
//! `cargo test -p faktor-server --features streaming-harness --lib \
//!  streaming_delivery -- --nocapture --test-threads=1`

use crate::api::tests::test_deps;
use crate::{serve_arc, ServerDeps};
use faktor_core::event::EventKind;
use faktor_core::state::AgentState;
use faktor_session::SessionHandle;
use futures_util::StreamExt as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ------------------------------------------------------------- SSE harness

#[derive(Debug, Default)]
struct StreamStats {
    frames: u64,
    delta_frames: u64,
    state_frames: u64,
    heartbeats: u64,
    bytes: u64,
    latencies_ms: Vec<u64>,
}

impl StreamStats {
    fn record(&mut self, frame: &Frame, received_ms: i64) {
        self.frames += 1;
        self.bytes += frame.bytes as u64;
        match frame.event.as_str() {
            "heartbeat" => {
                self.heartbeats += 1;
            }
            "model_chunk_received" => {
                self.delta_frames += 1;
                if let Some(ts) = frame.ts_ms {
                    self.latencies_ms
                        .push(received_ms.saturating_sub(ts).max(0) as u64);
                }
            }
            _ => {
                self.state_frames += 1;
                if let Some(ts) = frame.ts_ms {
                    self.latencies_ms
                        .push(received_ms.saturating_sub(ts).max(0) as u64);
                }
            }
        }
    }

    fn percentile(&mut self, p: usize) -> u64 {
        if self.latencies_ms.is_empty() {
            return 0;
        }
        self.latencies_ms.sort_unstable();
        let idx = ((self.latencies_ms.len() - 1) * p) / 100;
        self.latencies_ms[idx]
    }

    fn max_latency(&self) -> u64 {
        self.latencies_ms.iter().copied().max().unwrap_or(0)
    }
}

#[derive(Debug)]
struct Frame {
    id: u64,
    event: String,
    bytes: usize,
    ts_ms: Option<i64>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn extract_ts(data: &str) -> Option<i64> {
    let at = data.rfind("\"tsMs\":")? + "\"tsMs\":".len();
    let rest = data[at..].trim_start();
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

fn parse_frame(text: &str, bytes: usize) -> Option<Frame> {
    let mut id = None;
    let mut event = None;
    let mut data = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("id: ") {
            id = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("event: ") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data: ") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v);
        }
    }
    Some(Frame {
        id: id?,
        event: event?,
        bytes,
        ts_ms: extract_ts(&data),
    })
}

/// Stream `path` and invoke `on_frame` until it returns `false`. Frames are
/// dechunked by reqwest; a frame is dispatched as soon as its blank-line
/// terminator is observed, so the receive timestamp is the frame's arrival.
async fn stream_until(
    addr: std::net::SocketAddr,
    token: &str,
    path: &str,
    deadline: Duration,
    mut on_frame: impl FnMut(Frame) -> bool,
) {
    let client = reqwest::Client::builder().build().unwrap();
    let response = client
        .get(format!("http://{addr}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .expect("SSE connect");
    assert_eq!(response.status(), 200, "SSE status");
    let mut body = response.bytes_stream();
    let mut pending: Vec<u8> = Vec::new();
    let started = Instant::now();
    'outer: while let Some(chunk) = tokio::time::timeout(deadline, body.next())
        .await
        .unwrap_or_else(|_| panic!("SSE deadline {deadline:?} exceeded"))
    {
        let chunk = chunk.expect("SSE body chunk");
        pending.extend_from_slice(&chunk);
        loop {
            let Some(end) = pending.windows(2).position(|w| w == b"\n\n") else {
                if pending.len() > 2 * 1024 * 1024 {
                    panic!("unterminated SSE frame exceeded 2 MiB");
                }
                break;
            };
            let raw: Vec<u8> = pending.drain(..end + 2).collect();
            let bytes = raw.len();
            let text = String::from_utf8_lossy(&raw);
            if let Some(frame) = parse_frame(&text, bytes) {
                if !on_frame(frame) {
                    break 'outer;
                }
            }
        }
        if started.elapsed() > deadline {
            panic!("SSE deadline {deadline:?} exceeded");
        }
    }
}

fn seed_to_streaming(handle: &SessionHandle) {
    handle
        .force_append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
    handle
        .force_append_event(
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            Some(faktor_core::id::OpId::new(1)),
            Some(serde_json::json!({"message_id": 1, "text_len": 0})),
        )
        .unwrap();
}

fn producer_deltas(
    handle: SessionHandle,
    count: u64,
    per_second: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let interval = Duration::from_micros(1_000_000 / per_second.max(1));
        let started = Instant::now();
        for i in 0..count {
            handle
                .force_append_event(
                    EventKind::ModelChunkReceived,
                    AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({
                        "message_id": 1,
                        "text_len": 4,
                        "i": i,
                    })),
                )
                .expect("append delta");
            // Absolute pacing: the append cost is absorbed, so the offered
            // rate is the requested rate (not interval + append time).
            let due = started + interval * (i as u32 + 1);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
        }
    })
}

/// The SSE body is dropped from the client side; with HTTP/1 half-close
/// enabled the daemon notices a full close on its next bounded write (the
/// <= 5 s idle heartbeat), so the harness waits inside that bound plus
/// shutdown slack.
async fn wait_for_server_death(server: &crate::ServerHandle) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while server.is_alive() {
        assert!(
            Instant::now() < deadline,
            "the native serve task must terminate within the 15s harness bound"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn start_server(deps: ServerDeps) -> (crate::ServerHandle, String) {
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(Arc::new(deps), 0).await.unwrap();
    (server, token)
}

// ------------------------------------------------------------- scenarios

/// 200 deltas/s x 10_000 deltas against one live SSE reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_delivery_deltas_200ps_10k() {
    const COUNT: u64 = 10_000;
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let session = deps.session.clone();
    let ws = session.create_workspace("/stream-harness").unwrap();
    let sid = session
        .create_session(ws, "delta-harness", "fake", "m")
        .unwrap()
        .id();
    let handle = session.get_session(sid).unwrap().unwrap();
    seed_to_streaming(&handle);
    let (server, token) = start_server(deps).await;

    let producer = producer_deltas(handle, COUNT, 200);
    let started = Instant::now();
    let mut stats = StreamStats::default();
    let mut first_ms = None;
    let mut last_ms = None;
    let mut last_id = 0u64;
    let mut duplicate_or_gap = 0u64;
    stream_until(
        server.addr,
        &token,
        &format!("/native/session/{}/events?after=0", sid.raw()),
        Duration::from_secs(70),
        |frame| {
            let received = now_ms();
            if frame.event == "model_chunk_received" {
                if first_ms.is_none() {
                    first_ms = Some(received);
                }
                last_ms = Some(received);
                if last_id != 0 && frame.id != last_id + 1 {
                    duplicate_or_gap += 1;
                }
                last_id = frame.id;
            }
            stats.record(&frame, received);
            stats.delta_frames < COUNT
        },
    )
    .await;
    producer.await.unwrap();
    let elapsed = started.elapsed();
    let data_span = match (first_ms, last_ms) {
        (Some(a), Some(b)) if b > a => Duration::from_millis((b - a) as u64),
        _ => elapsed,
    };
    let p50 = stats.percentile(50);
    let p95 = stats.percentile(95);
    let max_ms = stats.max_latency();
    println!(
        "HARNESS deltas frames={} heartbeats={} bytes={} span_ms={} frames_per_s={:.1} bytes_per_s={:.1} p50_ms={} p95_ms={} max_ms={} gaps={}",
        stats.delta_frames,
        stats.heartbeats,
        stats.bytes,
        data_span.as_millis(),
        stats.delta_frames as f64 / data_span.as_secs_f64().max(0.001),
        stats.bytes as f64 / data_span.as_secs_f64().max(0.001),
        p50,
        p95,
        max_ms,
        duplicate_or_gap,
    );
    assert_eq!(stats.delta_frames, COUNT);
    assert_eq!(
        duplicate_or_gap, 0,
        "coalescing must preserve journal order"
    );
    server.request_shutdown();
    wait_for_server_death(&server).await;
}

/// 10_000-event catch-up with no writer active: every durable frame is
/// delivered once, in journal order, at the catch-up rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_delivery_catch_up_10k() {
    const COUNT: u64 = 10_000;
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let session = deps.session.clone();
    let ws = session.create_workspace("/stream-harness").unwrap();
    let sid = session
        .create_session(ws, "catch-up-harness", "fake", "m")
        .unwrap()
        .id();
    let handle = session.get_session(sid).unwrap().unwrap();
    seed_to_streaming(&handle);
    for i in 0..COUNT {
        handle
            .force_append_event(
                EventKind::ModelChunkReceived,
                AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({
                    "message_id": 1,
                    "text_len": 4,
                    "i": i,
                })),
            )
            .unwrap();
    }
    let (server, token) = start_server(deps).await;
    let started = Instant::now();
    let mut seen = 0u64;
    stream_until(
        server.addr,
        &token,
        &format!("/native/session/{}/events?after=0", sid.raw()),
        Duration::from_secs(60),
        |frame| {
            if frame.event == "model_chunk_received" {
                seen += 1;
            }
            seen < COUNT
        },
    )
    .await;
    let elapsed = started.elapsed();
    println!(
        "HARNESS catch_up frames={} span_ms={} frames_per_s={:.1}",
        seen,
        elapsed.as_millis(),
        seen as f64 / elapsed.as_secs_f64().max(0.001),
    );
    assert_eq!(seen, COUNT);
    server.request_shutdown();
    wait_for_server_death(&server).await;
}

/// 1_000 non-delta state events appended in one burst: they must flush
/// immediately (no coalescing linger) and keep journal order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_delivery_state_burst_1k() {
    const COUNT: u64 = 1_000;
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let session = deps.session.clone();
    let ws = session.create_workspace("/stream-harness").unwrap();
    let sid = session
        .create_session(ws, "state-harness", "fake", "m")
        .unwrap()
        .id();
    let handle = session.get_session(sid).unwrap().unwrap();
    seed_to_streaming(&handle);
    let (server, token) = start_server(deps).await;
    let producer = tokio::task::spawn_blocking(move || {
        for i in 0..COUNT {
            handle
                .force_append_event(
                    EventKind::PhaseChanged,
                    AgentState::WaitingForModel,
                    None,
                    Some(serde_json::json!({"i": i})),
                )
                .expect("append state event");
        }
    });
    let started = Instant::now();
    let mut stats = StreamStats::default();
    let mut last_id = 0u64;
    let mut gaps = 0u64;
    stream_until(
        server.addr,
        &token,
        &format!("/native/session/{}/events?after=0", sid.raw()),
        Duration::from_secs(30),
        |frame| {
            if frame.event == "phase_changed" {
                if last_id != 0 && frame.id != last_id + 1 {
                    gaps += 1;
                }
                last_id = frame.id;
            }
            stats.record(&frame, now_ms());
            stats.state_frames < COUNT
        },
    )
    .await;
    producer.await.unwrap();
    let elapsed = started.elapsed();
    let p50 = stats.percentile(50);
    let p95 = stats.percentile(95);
    let max_ms = stats.max_latency();
    println!(
        "HARNESS state_burst frames={} bytes={} span_ms={} frames_per_s={:.1} p50_ms={} p95_ms={} max_ms={} gaps={}",
        stats.state_frames,
        stats.bytes,
        elapsed.as_millis(),
        stats.state_frames as f64 / elapsed.as_secs_f64().max(0.001),
        p50,
        p95,
        max_ms,
        gaps,
    );
    assert_eq!(stats.state_frames, COUNT);
    assert_eq!(gaps, 0);
    server.request_shutdown();
    wait_for_server_death(&server).await;
}

/// 10_000 terminal lifetime rows walked with `after` pages of 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_delivery_terminal_10k_lines() {
    const COUNT: usize = 10_000;
    const PAGE: usize = 200;
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let session = deps.session.clone();
    let ws = session.create_workspace("/stream-harness").unwrap();
    let sid = session
        .create_session(ws, "terminal-harness", "fake", "m")
        .unwrap()
        .id();
    let handle = session.get_session(sid).unwrap().unwrap();
    let (server, token) = start_server(deps).await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", server.addr);
    // Run the lazy recovery scan once before planting rows.
    let first = client
        .get(format!(
            "{base}/native/session/{}/terminal/events?after=0",
            sid.raw()
        ))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    drop(first);
    let now = session.now_ms();
    for i in 0..COUNT {
        let row = faktor_session::TerminalDurableRow {
            terminal_id: format!("t{i}"),
            session_id: sid.raw(),
            task_id: 1,
            agent_id: None,
            operation_id: 1,
            pid: 40_000 + i as u32,
            start_time_ms: now,
            at_ms: now,
            execution_profile: String::new(),
        };
        handle.ledger_terminal_created(&row).unwrap();
    }
    let started = Instant::now();
    let mut cursor = 0u64;
    let mut seen = 0usize;
    let mut bytes = 0usize;
    let mut pages = 0usize;
    let mut page_ms: Vec<u128> = Vec::new();
    loop {
        let page_started = Instant::now();
        let response = client
            .get(format!(
                "{base}/native/session/{}/terminal/events?after={cursor}&limit={PAGE}",
                sid.raw()
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        bytes += body.to_string().len();
        pages += 1;
        page_ms.push(page_started.elapsed().as_millis());
        let events = body["events"].as_array().unwrap();
        for event in events {
            let id = event["id"].as_u64().unwrap();
            assert!(id > cursor, "cursor must be strictly exclusive");
            cursor = id;
            seen += 1;
        }
        if !body["hasMore"].as_bool().unwrap() {
            break;
        }
    }
    let elapsed = started.elapsed();
    page_ms.sort_unstable();
    println!(
        "HARNESS terminal lines={} pages={} bytes={} span_ms={} lines_per_s={:.1} page_p50_ms={} page_p95_ms={} page_max_ms={}",
        seen,
        pages,
        bytes,
        elapsed.as_millis(),
        seen as f64 / elapsed.as_secs_f64().max(0.001),
        page_ms[page_ms.len() / 2],
        page_ms[(page_ms.len() * 95) / 100],
        page_ms.last().copied().unwrap_or(0),
    );
    assert_eq!(seen, COUNT);
    server.request_shutdown();
    wait_for_server_death(&server).await;
}
