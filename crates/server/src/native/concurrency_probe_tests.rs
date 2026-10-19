//! New-angle concurrency probe: 64 durable sessions, 32 live SSE replays and
//! 16 parallel read workers on ONE real loopback listener. This is a bounded
//! burst (no soak loop): every stream must receive every event exactly once,
//! the read workers must answer without errors, tail latency is compared to
//! the same request measured alone, and resident memory/WAL stay bounded.
//!
//! Standing suite: the assertions are relative (baseline p95 x generous
//! factor + a real floor) so a slow/loaded CI box does not make it flaky,
//! while a regression to per-request journal verification, global-watch
//! fan-out or per-message parts queries blows the ceiling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_core::event::EventKind;
use faktor_core::state::AgentState;
use faktor_session::messages::PartKind;
use futures_util::StreamExt as _;

use crate::api::tests::test_deps;
use crate::serve;

const SESSIONS: usize = 64;
const STREAMS: usize = 32;
const READ_WORKERS: usize = 16;
const REQUESTS_PER_WORKER: usize = 20;
const DELTAS: usize = 10;

fn percentile(samples: &[Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn rss_kib() -> u64 {
    let pid = std::process::id().to_string();
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok();
    output
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Read one SSE stream until `expected` non-heartbeat frames arrived, then
/// return their frame ids (the durable journal seq).
async fn read_stream(
    client: reqwest::Client,
    base: String,
    token: String,
    session: u64,
    after: u64,
    expected: usize,
    started: Arc<tokio::sync::Barrier>,
) -> (Vec<u64>, Vec<u128>) {
    let response = client
        .get(format!(
            "{base}/native/session/{session}/events?after={after}"
        ))
        .bearer_auth(&token)
        .send()
        .await
        .expect("SSE connect");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    // Headers are out: the body is now subscribed to this session's journal
    // watch. The main task releases the burst only after ALL 32 arrived.
    started.wait().await;
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut ids = Vec::with_capacity(expected);
    let mut lags_ms = Vec::with_capacity(expected);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("SSE chunk");
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(end) = buffer.find("\n\n") {
            let frame: String = buffer.drain(..end + 2).collect();
            let mut id = None;
            let mut event = None;
            let mut ts_ms = None;
            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("id: ") {
                    id = value.trim().parse::<u64>().ok();
                } else if let Some(value) = line.strip_prefix("event: ") {
                    event = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    ts_ms = serde_json::from_str::<serde_json::Value>(value.trim())
                        .ok()
                        .and_then(|data| data.get("tsMs").and_then(|v| v.as_i64()));
                }
            }
            if matches!(event.as_deref(), Some("heartbeat")) {
                continue;
            }
            if let Some(id) = id {
                if let Some(ts_ms) = ts_ms {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(ts_ms);
                    lags_ms.push(now.saturating_sub(ts_ms) as u128);
                }
                ids.push(id);
                if ids.len() >= expected {
                    return (ids, lags_ms);
                }
            }
        }
    }
    (ids, lags_ms)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixty_four_sessions_thirty_two_streams_and_sixteen_readers_stay_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.as_str().to_string();
    let manager = deps.session.clone();
    let store_root = manager.store().root().to_path_buf();

    // Seed 64 durable sessions. Each carries a short journal (4 events) and
    // three messages with two parts each, so the read workers page real rows.
    let workspace = manager.create_workspace("/w").unwrap();
    let mut cursors: Vec<(u64, u64)> = Vec::with_capacity(SESSIONS);
    for index in 0..SESSIONS {
        let handle = manager
            .create_session(workspace, &format!("probe-{index}"), "fake", "m")
            .unwrap();
        for _ in 0..4 {
            handle
                .force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
                .unwrap();
        }
        for seq in 1..=3i64 {
            let message = handle
                .put_message(seq, "user", serde_json::json!({"text": "turn"}))
                .unwrap();
            handle
                .put_part(message, PartKind::Text, serde_json::json!({"text": "a"}))
                .unwrap();
            handle
                .put_part(message, PartKind::Text, serde_json::json!({"text": "b"}))
                .unwrap();
        }
        let after = handle.last_event_seq().unwrap().unwrap().raw();
        cursors.push((handle.id().raw(), after));
    }
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let client = reqwest::Client::new();
    let rss_before = rss_kib();

    // Baseline: the same request, alone.
    let baseline_session = cursors[0].0;
    let mut baseline = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let response = client
            .get(format!(
                "{base}/native/messages?session={baseline_session}&limit=50"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let _: serde_json::Value = response.json().await.unwrap();
        baseline.push(started.elapsed());
    }
    let baseline_p95 = percentile(&baseline, 0.95);

    // 32 concurrent streams (each on its own session) + 16 read workers.
    let ready = Arc::new(tokio::sync::Barrier::new(STREAMS + 1));
    let mut streams = tokio::task::JoinSet::new();
    for (session, after) in cursors.iter().copied().take(STREAMS) {
        streams.spawn(read_stream(
            client.clone(),
            base.clone(),
            token.clone(),
            session,
            after,
            DELTAS,
            Arc::clone(&ready),
        ));
    }
    let mut workers = tokio::task::JoinSet::new();
    for worker in 0..READ_WORKERS {
        let client = client.clone();
        let base = base.clone();
        let token = token.clone();
        let cursors = cursors.clone();
        workers.spawn(async move {
            let mut samples = Vec::with_capacity(REQUESTS_PER_WORKER);
            for request in 0..REQUESTS_PER_WORKER {
                let (session, _) = cursors[(worker * 7 + request) % cursors.len()];
                let path = if request % 3 == 0 {
                    format!("{base}/native/events?session={session}&limit=20")
                } else if request % 3 == 1 {
                    format!("{base}/native/messages?session={session}&limit=50")
                } else {
                    format!("{base}/native/providers")
                };
                let started = Instant::now();
                let response = client
                    .get(path)
                    .bearer_auth(&token)
                    .send()
                    .await
                    .expect("read request");
                assert_eq!(
                    response.status(),
                    reqwest::StatusCode::OK,
                    "read worker {} request {request}",
                    worker
                );
                let _: serde_json::Value = response.json().await.expect("json body");
                samples.push(started.elapsed());
            }
            samples
        });
    }

    // The live delta burst starts only once every stream body is attached,
    // so measured lag is append -> frame arrival, not replay backlog age.
    ready.wait().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    for (session, _) in &cursors {
        let handle = manager
            .get_session(faktor_core::id::SessionId::new(*session))
            .unwrap()
            .unwrap();
        for _ in 0..DELTAS {
            handle
                .force_append_event(EventKind::ModelChunkReceived, AgentState::Idle, None, None)
                .unwrap();
        }
    }

    let mut stream_ids = Vec::with_capacity(STREAMS);
    let mut event_lags_ms = Vec::with_capacity(STREAMS * DELTAS);
    while let Some(result) = streams.join_next().await {
        let (ids, lags) = result.unwrap();
        stream_ids.push(ids);
        event_lags_ms.extend(lags);
    }
    let mut load_samples = Vec::with_capacity(READ_WORKERS * REQUESTS_PER_WORKER);
    while let Some(result) = workers.join_next().await {
        load_samples.extend(result.unwrap());
    }

    // Zero drops/duplicates: every stream observed its 10 new events in
    // strictly increasing journal order.
    for ids in &stream_ids {
        assert_eq!(
            ids.len(),
            DELTAS,
            "stream lost or duplicated frames: {ids:?}"
        );
        for pair in ids.windows(2) {
            assert!(pair[0] < pair[1], "stream reordered frames: {ids:?}");
        }
    }
    let p50 = percentile(&load_samples, 0.50);
    let p95 = percentile(&load_samples, 0.95);
    let p99 = percentile(&load_samples, 0.99);
    let max = *load_samples.iter().max().unwrap();
    event_lags_ms.sort_unstable();
    let lag_p95 = event_lags_ms
        .get(((event_lags_ms.len() as f64 - 1.0) * 0.95).round() as usize)
        .copied()
        .unwrap_or(0);
    let lag_max = event_lags_ms.last().copied().unwrap_or(0);
    assert!(
        lag_p95 < 2_000,
        "SSE event lag p95 {lag_p95}ms exceeds the 2s ceiling"
    );
    let rss_after = rss_kib();
    let wal_bytes = std::fs::metadata(store_root.join("faktor-plus.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "[perf] 64 sessions / 32 SSE / {} reads: baseline p95={:?} load p50={:?} p95={:?} \
         p99={:?} max={:?}; sse lag p95={lag_p95}ms max={lag_max}ms; rss {} -> {} KiB; wal {} KiB",
        load_samples.len(),
        baseline_p95,
        p50,
        p95,
        p99,
        max,
        rss_before,
        rss_after,
        wal_bytes / 1024
    );

    // Generous but real: the tail may slow by 40x the isolated request (or
    // by an absolute 1.5s on a loaded machine), never seconds per request.
    let ceiling = baseline_p95
        .saturating_mul(40)
        .max(Duration::from_millis(1_500));
    assert!(
        p95 <= ceiling,
        "load p95 {p95:?} exceeds the relative ceiling {ceiling:?} (baseline {baseline_p95:?})"
    );
    assert!(
        p99 <= ceiling.saturating_mul(2),
        "load p99 {p99:?} exceeds 2x the ceiling {ceiling:?}"
    );
    let rss_delta = rss_after.saturating_sub(rss_before);
    assert!(
        rss_delta < 256 * 1024,
        "resident memory grew by {rss_delta} KiB during the burst"
    );
    assert!(
        wal_bytes < 64 * 1024 * 1024,
        "WAL grew to {wal_bytes} bytes during the burst"
    );
    handle.request_shutdown();
}
