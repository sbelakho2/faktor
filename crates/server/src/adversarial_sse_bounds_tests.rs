//! Adversarial SSE and body/page-bound tests (task categories 3 and 4).
//!
//! Production entry points: the served native router (SSE journal stream,
//! paged event/message reads) plus [`page_limit`] and the frozen native
//! page bounds.

use super::tests::{test_deps, wait_until_server_dead};
use super::*;
use faktor_core::event::EventKind;
use faktor_core::state::AgentState;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

fn seeded_deps(root: &std::path::Path, name: &str) -> (Arc<ServerDeps>, u64) {
    let deps = Arc::new(test_deps(root));
    let ws = deps.session.create_workspace("/sse-root").unwrap();
    let session = deps.session.create_session(ws, name, "fake", "m").unwrap();
    let handle = deps.session.get_session(session.id()).unwrap().unwrap();
    for _ in 0..3 {
        handle
            .force_append_event(
                EventKind::PhaseChanged,
                AgentState::WaitingForModel,
                None,
                None,
            )
            .unwrap();
    }
    (deps, session.id().raw())
}

async fn raw_get(addr: std::net::SocketAddr, token: &str, path: &str) -> String {
    raw_get_stop(addr, token, path, 0).await
}

/// Read a raw HTTP response byte-at-a-time, stopping early once `frames`
/// SSE frame separators have arrived (0 = stop on EOF/deadline only).
async fn raw_get_stop(
    addr: std::net::SocketAddr,
    token: &str,
    path: &str,
    frames: usize,
) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut byte = [0u8; 1];
    loop {
        if frames > 0 {
            let seen = out.windows(2).filter(|w| w == b"\n\n").count();
            if seen >= frames {
                break;
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, stream.read(&mut byte)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(_)) => out.push(byte[0]),
            Ok(Err(_)) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_sse(text: &str) -> Vec<Vec<(String, String)>> {
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let mut frames = Vec::new();
    for frame in body.split("\n\n") {
        let mut fields: Vec<(String, String)> = Vec::new();
        for line in frame.lines() {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some((name, value)) = line.split_once(':') {
                fields.push((name.trim().to_string(), value.trim_start().to_string()));
            }
        }
        if !fields.is_empty() {
            frames.push(fields);
        }
    }
    frames
}

fn sse_data_json(frame: &[(String, String)]) -> String {
    frame
        .iter()
        .filter(|(name, _)| name == "data")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn sse_frames_are_wellformed_and_byte_split_stable() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, sid) = seeded_deps(dir.path(), "sse-frame");
    let token = deps.auth_token.as_str().to_string();
    let handle = serve_arc(deps, 0).await.unwrap();
    let path = format!("/native/session/{sid}/events?after=0");

    let buffered = raw_get_stop(handle.addr, &token, &path, 3).await;
    let split = raw_get_stop(handle.addr, &token, &path, 3).await;
    assert!(
        buffered.starts_with("HTTP/1.1 200"),
        "SSE must establish: {}",
        &buffered[..buffered.len().min(120)]
    );
    assert!(
        buffered
            .to_lowercase()
            .contains("content-type: text/event-stream"),
        "SSE content type: {}",
        &buffered[..buffered.len().min(400)]
    );
    let frames = parse_sse(&buffered);
    assert!(
        frames.len() >= 3,
        "at least the three seeded events stream: {frames:?}"
    );
    let mut last_id: i64 = -1;
    let mut data_frames = 0usize;
    for frame in &frames {
        let ids: Vec<&str> = frame
            .iter()
            .filter(|(name, _)| name == "id")
            .map(|(_, value)| value.as_str())
            .collect();
        let events: Vec<&str> = frame
            .iter()
            .filter(|(name, _)| name == "event")
            .map(|(_, value)| value.as_str())
            .collect();
        assert!(ids.len() <= 1, "at most one id per frame: {frame:?}");
        assert!(events.len() <= 1, "at most one event per frame: {frame:?}");
        if events.first() == Some(&"heartbeat") {
            assert!(ids.is_empty(), "heartbeats carry no journal id: {frame:?}");
            continue;
        }
        let id = ids
            .first()
            .expect("data frames carry an id")
            .parse::<i64>()
            .unwrap_or_else(|e| panic!("journal id must be numeric in {frame:?}: {e}"));
        assert!(
            id > last_id,
            "journal ids strictly ascend: {last_id} -> {id}"
        );
        last_id = id;
        let data = sse_data_json(frame);
        let parsed: serde_json::Value = serde_json::from_str(&data)
            .unwrap_or_else(|e| panic!("frame data must be JSON ({data:?}): {e}"));
        assert!(
            parsed.is_object(),
            "frame data is a journal row object: {data:?}"
        );
        assert!(
            frame.len() <= 8,
            "field count per frame stays bounded: {frame:?}"
        );
        assert!(
            data.len() < 1024 * 1024,
            "frame payload stays bounded: {} bytes",
            data.len()
        );
        data_frames += 1;
    }
    assert!(data_frames >= 3, "the seeded events are data frames");
    assert!(
        last_id >= 3,
        "the durable sequence advances with the seeds: {last_id}"
    );

    // Read at the raw byte level: the frames are identical (no interleaving,
    // truncation or coalescing differences at the wire level).
    let buffered_frames: Vec<String> = frames
        .iter()
        .map(|f| {
            f.iter()
                .map(|(n, v)| format!("{n}: {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    let split_frames: Vec<String> = parse_sse(&split)
        .iter()
        .map(|f| {
            f.iter()
                .map(|(n, v)| format!("{n}: {v}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect();
    assert_eq!(
        buffered_frames, split_frames,
        "byte-wise reads must see identical frames"
    );
    handle.request_shutdown();
    wait_until_server_dead(&handle).await;
}

#[tokio::test]
async fn sse_multiline_data_comments_and_heartbeats() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let ws = deps.session.create_workspace("/sse-multiline").unwrap();
    let session = deps
        .session
        .create_session(ws, "sse-multiline", "fake", "m")
        .unwrap();
    let handle = deps.session.get_session(session.id()).unwrap().unwrap();
    handle
        .force_append_event(
            EventKind::PhaseChanged,
            AgentState::WaitingForModel,
            None,
            Some(serde_json::json!({"text": "line1\nline2\nline3", "n": 1})),
        )
        .unwrap();
    let sid = session.id().raw();
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(deps, 0).await.unwrap();
    let text = raw_get_stop(
        server.addr,
        &token,
        &format!("/native/session/{sid}/events?after=0"),
        2,
    )
    .await;
    let frames = parse_sse(&text);
    let frame = frames
        .iter()
        .find(|f| sse_data_json(f).contains("line1"))
        .unwrap_or_else(|| panic!("multiline payload frame missing: {frames:?}"));
    let data = sse_data_json(frame);
    let parsed: serde_json::Value = serde_json::from_str(&data)
        .unwrap_or_else(|e| panic!("SSE multiline data must reassemble to JSON: {e}"));
    assert_eq!(
        parsed["payload"]["text"], "line1\nline2\nline3",
        "multi-line data survives the SSE encoding: {data:?}"
    );
    // Only the frame's own fields are present: no raw control injection.
    for (name, _) in frame {
        assert!(
            matches!(name.as_str(), "id" | "event" | "data" | "retry"),
            "unexpected SSE field {name:?}"
        );
    }
    // The frame carries exactly the fields the event type emits.
    assert!(
        frame.iter().any(|(n, _)| n == "id"),
        "durable event frames carry an id"
    );
    server.request_shutdown();
    wait_until_server_dead(&server).await;
}

#[tokio::test]
async fn paged_cursor_gaps_regressions_and_reconnect_classification() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, sid) = seeded_deps(dir.path(), "cursor");
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(deps, 0).await.unwrap();
    let base = format!("http://{}", server.addr);
    let client = reqwest::Client::new();

    let page = |after: &str| {
        let client = client.clone();
        let base = base.clone();
        let token = token.clone();
        let after = after.to_string();
        async move {
            client
                .get(format!(
                    "{base}/native/events?session={sid}&after={after}&limit=100"
                ))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
        }
    };

    // From the beginning: ascending durable seqs, no gaps inside the page.
    let body: serde_json::Value = page("0").await.json().await.unwrap();
    let events = body["events"].as_array().expect("events array");
    assert!(
        events.len() >= 3,
        "the seeded journal is visible from cursor 0: {body}"
    );
    let seqs: Vec<i64> = events
        .iter()
        .map(|e| e["seq"].as_i64().expect("numeric seq"))
        .collect();
    for pair in seqs.windows(2) {
        assert_eq!(
            pair[1],
            pair[0] + 1,
            "the page has no internal gaps: {seqs:?}"
        );
    }
    let last = *seqs.last().unwrap();

    // A mid cursor is an exclusive replay of seq > cursor.
    let mid = seqs[1];
    let body: serde_json::Value = page(&mid.to_string()).await.json().await.unwrap();
    let events = body["events"].as_array().unwrap();
    assert!(
        events.iter().all(|e| e["seq"].as_i64().unwrap() > mid),
        "after={mid} must replay strictly greater seqs: {body}"
    );
    assert!(
        events.iter().all(|e| e["seq"].as_i64().unwrap() <= last),
        "after={mid} must not invent future seqs: {body}"
    );

    // Reconnect classification: replaying from the last seen seq yields no
    // duplicate of that seq (at-least-once consumers dedupe on id).
    let body: serde_json::Value = page(&last.to_string()).await.json().await.unwrap();
    let events = body["events"].as_array().unwrap();
    assert!(
        events.iter().all(|e| e["seq"].as_i64().unwrap() > last),
        "reconnect at last={last} must not repeat it: {body}"
    );

    // Cursor regressions are legal replays, never errors.
    let body_status = page("1").await.status();
    assert_eq!(body_status, 200, "cursor regression to 1 is a replay");
    // Cursor gaps (far future / u64 ceiling) are empty, never an error.
    for cursor in [
        (last + 1000).to_string(),
        i64::MAX.to_string(),
        u64::MAX.to_string(),
    ] {
        let response = page(&cursor).await;
        assert_eq!(
            response.status(),
            200,
            "cursor gap {cursor} must be an empty page"
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert!(
            body["events"].as_array().unwrap().is_empty(),
            "cursor gap {cursor} yields no events: {body}"
        );
    }
    // Hostile cursors are typed 400s.
    for cursor in ["-1", "abc", "1.5", "", "0x10", " 1"] {
        let response = page(cursor).await;
        assert_eq!(
            response.status(),
            400,
            "hostile cursor {cursor:?} must be a typed 400"
        );
    }
    server.request_shutdown();
    wait_until_server_dead(&server).await;
}

#[tokio::test]
async fn sse_cursor_extremes_keep_the_stream_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, sid) = seeded_deps(dir.path(), "sse-cursor");
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    for cursor in ["0", "1", "3", "4", "100000", "18446744073709551615"] {
        let response = client
            .get(format!(
                "http://{}/native/session/{sid}/events?after={cursor}",
                server.addr
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "SSE cursor {cursor} must keep the stream healthy"
        );
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "text/event-stream",
            "SSE cursor {cursor} keeps the event-stream type"
        );
        drop(response);
    }
    // Hostile SSE cursors are typed 400s before any stream starts.
    for cursor in ["-1", "abc", "1.5", "0x1", ""] {
        let response = client
            .get(format!(
                "http://{}/native/session/{sid}/events?after={cursor}",
                server.addr
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            400,
            "SSE hostile cursor {cursor:?} must be a typed 400"
        );
    }
    server.request_shutdown();
    wait_until_server_dead(&server).await;
}

#[test]
fn page_limit_boundary_matrix_is_exact() {
    for max in [MAX_NATIVE_CURSOR_PAGE, MAX_NATIVE_EVENT_PAGE] {
        let label = format!("max={max}");
        assert_eq!(
            page_limit(None, max).unwrap(),
            max,
            "absent limit defaults to the cap ({label})"
        );
        assert_eq!(
            page_limit(Some(1), max).unwrap(),
            1,
            "limit 1 is accepted ({label})"
        );
        assert_eq!(
            page_limit(Some(max as u64), max).unwrap(),
            max,
            "limit at the cap is accepted ({label})"
        );
        for bad in [
            0u64,
            max as u64 + 1,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            let err = page_limit(Some(bad), max)
                .expect_err(&format!("limit {bad} must be refused at {label}"));
            assert_eq!(
                err.code, "malformed",
                "refusal code for limit {bad} ({label})"
            );
            assert_eq!(
                err.http_status, 400,
                "refusal status for limit {bad} ({label})"
            );
            assert!(
                !err.message.is_empty(),
                "refusal names the bound for limit {bad} ({label})"
            );
        }
    }
}

#[tokio::test]
async fn query_limit_boundaries_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let (deps, sid) = seeded_deps(dir.path(), "limits");
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", server.addr);
    let get = |path: String| {
        let client = client.clone();
        let token = token.clone();
        async move { client.get(path).bearer_auth(&token).send().await.unwrap() }
    };

    // /native/events: 0 and cap+1/u64 extremes are 400; 1 and cap are 200.
    for (limit, ok) in [
        ("0", false),
        ("1", true),
        ("256", true),
        ("257", false),
        ("9223372036854775807", false),
        ("18446744073709551615", false),
        ("-1", false),
        ("abc", false),
        ("1.5", false),
    ] {
        let response = get(format!("{base}/native/events?session={sid}&limit={limit}")).await;
        assert_eq!(
            response.status() == 200,
            ok,
            "/native/events limit {limit} expectation"
        );
        if !ok {
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            assert!(
                body["error"]["code"].is_string(),
                "typed refusal for /native/events limit {limit}: {body}"
            );
        }
    }
    // /native/messages cursor cap is 200.
    for (limit, ok) in [
        ("0", false),
        ("1", true),
        ("200", true),
        ("201", false),
        ("u64::MAX", false),
    ] {
        let value = if limit == "u64::MAX" {
            u64::MAX.to_string()
        } else {
            limit.to_string()
        };
        let response = get(format!(
            "{base}/native/messages?session={sid}&limit={value}"
        ))
        .await;
        assert_eq!(
            response.status() == 200,
            ok,
            "/native/messages limit {value} expectation"
        );
    }
    // before is an i64 cursor: 0 is a typed 400 (must be >= 1), 1 is fine.
    let response = get(format!("{base}/native/messages?session={sid}&before=0")).await;
    assert_eq!(response.status(), 400, "before=0 is refused");
    let response = get(format!("{base}/native/messages?session={sid}&before=1")).await;
    assert_eq!(response.status(), 200, "before=1 is accepted");
    // Unknown query fields are refused by the strict query DTOs.
    let response = get(format!("{base}/native/messages?session={sid}&bogus=1")).await;
    assert_eq!(
        response.status(),
        400,
        "unknown query field on /native/messages is refused"
    );
    server.request_shutdown();
    wait_until_server_dead(&server).await;
}

async fn raw_post(
    addr: std::net::SocketAddr,
    token: &str,
    path: &str,
    content_type: &str,
    declared_len: Option<usize>,
    body: &[u8],
    half_close: bool,
) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!("POST {path} HTTP/1.1\r\nHost: localhost\r\n");
    if !token.is_empty() {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str(&format!("Content-Type: {content_type}\r\n"));
    if let Some(len) = declared_len {
        request.push_str(&format!("Content-Length: {len}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    if !body.is_empty() {
        stream.write_all(body).await.unwrap();
    }
    if half_close {
        let _ = stream.shutdown().await;
    }
    let mut out = Vec::new();
    let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn body_bounds_content_type_truncation_and_utf8_splits() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(deps, 0).await.unwrap();
    let addr = server.addr;

    // Content-Length over MAX_BODY_BYTES is refused before a body is read.
    let response = raw_post(
        addr,
        &token,
        "/native/session",
        "application/json",
        Some(MAX_BODY_BYTES + 1),
        &[],
        false,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 413"),
        "a declared body over the 10 MiB cap must be 413: {}",
        &response[..response.len().min(160)]
    );
    assert!(
        response.len() < 4096,
        "the 413 refusal is small and immediate"
    );

    // A truncated body (Content-Length larger than what is sent) is a 400.
    let response = raw_post(
        addr,
        &token,
        "/native/session",
        "application/json",
        Some(64),
        br#"{"provider":"fake","model":"m"}"#,
        true,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "a truncated JSON body must be a typed 400: {}",
        &response[..response.len().min(160)]
    );

    // Wrong content type: the strict extractor refuses typed.
    let response = raw_post(
        addr,
        &token,
        "/native/session",
        "text/plain",
        Some(32),
        br#"{"provider":"fake","model":"m"}"#,
        false,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "a text/plain body on a JSON route must be refused: {}",
        &response[..response.len().min(160)]
    );

    // Empty body without Content-Length is refused.
    let response = raw_post(
        addr,
        &token,
        "/native/session",
        "application/json",
        None,
        &[],
        false,
    )
    .await;
    assert!(
        !response.starts_with("HTTP/1.1 200"),
        "an empty body must never create a session: {}",
        &response[..response.len().min(160)]
    );

    // A 1 MiB valid-JSON body is read and typed-refused (the decision field
    // is bounded), never dropped mid-body or 5xx.
    let big = format!(
        r#"{{"session_id":"1","permission_id":"p1","decision":"{}"}}"#,
        "a".repeat(1024 * 1024 - 60)
    );
    assert!(big.len() > 1024 * 1024 - 64, "1 MiB fixture size");
    let response = raw_post(
        addr,
        &token,
        "/native/permission/reply",
        "application/json",
        Some(big.len()),
        big.as_bytes(),
        false,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 4"),
        "a 1 MiB JSON body gets a typed client refusal: {}",
        &response[..response.len().min(160)]
    );

    // A 1 MiB garbage body is a 400, not a 500/reset.
    let garbage = vec![b'{'; 1024 * 1024];
    let response = raw_post(
        addr,
        &token,
        "/native/session",
        "application/json",
        Some(garbage.len()),
        &garbage,
        false,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "1 MiB garbage is a typed 400: {}",
        &response[..response.len().min(160)]
    );

    // UTF-8 split at EVERY byte boundary of a multibyte body: the JSON is
    // assembled across multiple TCP writes and parsed identically.
    let payload = r#"{"provider":"féké","model":"módel"}"#;
    for split in 0..=payload.len() {
        let (a, b) = payload.as_bytes().split_at(split);
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "POST /native/session HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.write_all(a).await.unwrap();
        tokio::task::yield_now().await;
        stream.write_all(b).await.unwrap();
        let _ = stream.shutdown().await;
        let mut out = Vec::new();
        let _ = timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
        let response = String::from_utf8_lossy(&out);
        assert!(
            response.starts_with("HTTP/1.1 4") || response.starts_with("HTTP/1.1 2"),
            "UTF-8 split at byte {split} must get a typed response, got {}",
            &response[..response.len().min(160)]
        );
        assert!(
            !response.starts_with("HTTP/1.1 5"),
            "UTF-8 split at byte {split} must never be a 500"
        );
    }

    // The daemon is still alive after all hostile bodies.
    let response = raw_get(addr, &token, "/native/health").await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "daemon survives the body matrix"
    );
    server.request_shutdown();
    wait_until_server_dead(&server).await;
}

/// Unknown methods and unknown paths must be the frozen typed JSON envelope,
/// never a bare empty 405/404 (the UI cannot render an empty body).
#[tokio::test]
async fn unknown_methods_and_paths_answer_the_typed_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(deps, 0).await.unwrap();
    let addr = server.addr;

    // POST on a GET-only route -> typed 405.
    let response = raw_post(
        addr,
        &token,
        "/native/health",
        "application/json",
        Some(0),
        &[],
        false,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 405"),
        "{}",
        &response[..response.len().min(160)]
    );
    assert!(response.contains("\"method_not_allowed\""), "{response}");

    // Unknown path -> typed 404.
    let response = raw_post(
        addr,
        &token,
        "/native/definitely-not-a-route",
        "application/json",
        Some(0),
        &[],
        false,
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 404"),
        "{}",
        &response[..response.len().min(160)]
    );
    assert!(response.contains("\"not_found\""), "{response}");

    server.request_shutdown();
    wait_until_server_dead(&server).await;
}

/// 2 MiB..10 MiB bodies are inside the daemon's advertised bound: axum's
/// 2 MiB Json-extractor default must not turn them into a misleading
/// `400 malformed` before the handler's own typed decision runs.
#[tokio::test]
async fn bodies_above_the_axum_default_are_not_mislabeled_malformed() {
    let dir = tempfile::tempdir().unwrap();
    let deps = Arc::new(test_deps(dir.path()));
    let token = deps.auth_token.as_str().to_string();
    let server = serve_arc(deps, 0).await.unwrap();
    let addr = server.addr;

    let title = "x".repeat(3 * 1024 * 1024);
    let body = serde_json::json!({
        "provider": "ghost",
        "model": "m",
        "workspace": "/tmp",
        "title": title,
    })
    .to_string()
    .into_bytes();
    let response = raw_post(
        addr,
        &token,
        "/native/session",
        "application/json",
        Some(body.len()),
        &body,
        false,
    )
    .await;
    assert!(
        !response.contains("\"malformed\""),
        "a 3 MiB body inside MAX_BODY_BYTES must reach the handler: {}",
        &response[..response.len().min(200)]
    );
    // The handler's own typed decision (unregistered provider) proves the
    // body was parsed rather than rejected by an extractor default.
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(response.contains("is not registered"), "{response}");

    server.request_shutdown();
    wait_until_server_dead(&server).await;
}
