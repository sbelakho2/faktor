//! `session_tests`: out-of-line slice of the native session test module.
//!
//! Kept out of `session.rs` so the production module stays under the
//! repository source-size ceiling.

use super::*;
use std::sync::Arc;

fn test_state(root: &std::path::Path, title: &str) -> (AppState, faktor_core::id::SessionId) {
    let deps = crate::api::tests::test_deps(root);
    let session = deps.session.clone();
    let state = AppState {
        deps: Arc::new(deps),
        auth: Arc::new(std::sync::RwLock::new(None)),
        terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let ws = session.create_workspace("/tmp").unwrap();
    let sid = session.create_session(ws, title, "fake", "m").unwrap().id();
    (state, sid)
}

fn abort_state(root: &std::path::Path) -> (AppState, faktor_core::id::SessionId) {
    test_state(root, "abort")
}

#[test]
fn ready_for_next_turn_is_not_a_running_task_view() {
    use faktor_core::state::AgentState;
    let done = vec!["step".to_string()];
    assert_eq!(
        super::task_view_state_tag(None, AgentState::ReadyForNextTurn, &[], &done, "goal"),
        "done",
        "a ready-between-turns session with completed work is done, not running"
    );
    assert_eq!(
        super::task_view_state_tag(None, AgentState::ReadyForNextTurn, &[], &[], "goal"),
        "in_progress"
    );
    assert_eq!(
        super::task_view_state_tag(None, AgentState::Streaming, &[], &[], "goal"),
        "running",
        "a genuinely live machine still projects running"
    );
}

#[test]
fn the_durable_task_state_wins_over_the_session_tag() {
    use faktor_core::state::{AgentState, TaskState};
    assert_eq!(
        super::task_view_state_tag(
            Some(TaskState::Verifying),
            AgentState::ReadyForNextTurn,
            &[],
            &[],
            "goal"
        ),
        "verifying",
        "a verifying task must never be flattened to the ready session tag"
    );
    assert_eq!(
        super::task_view_state_tag(
            Some(TaskState::NeedsVerification),
            AgentState::ReadyForNextTurn,
            &[],
            &[],
            "goal"
        ),
        "needs_verification"
    );
    assert_eq!(
        super::task_view_state_tag(
            Some(TaskState::VerifiedComplete),
            AgentState::ReadyForNextTurn,
            &[],
            &[],
            "goal"
        ),
        "verified_complete"
    );
    assert_eq!(
        super::task_view_state_tag(
            Some(TaskState::Failed),
            AgentState::ReadyForNextTurn,
            &[],
            &[],
            "goal"
        ),
        "failed"
    );
}

#[tokio::test]
async fn create_session_refuses_an_unregistered_provider_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = test_state(dir.path(), "seed");
    let before = state.deps.session.list_sessions(None).unwrap().len();
    let mut headers = authed_headers(&state);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    let response = super::native_create_session(
        State(state.clone()),
        headers,
        Ok(Json(NativeCreateSessionRequest {
            provider: "ghost".into(),
            model: "m".into(),
            workspace: Some("/tmp".into()),
            title: None,
        })),
    )
    .await;
    let (status, body) = json_body(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not registered"),
        "{body}"
    );
    assert_eq!(
        state.deps.session.list_sessions(None).unwrap().len(),
        before,
        "an unregistered provider must leave no session behind"
    );
}

/// Perf-wave follow-up (residual): the listing route reads the session
/// table exactly ONCE per page. The historical shape ran the manager's
/// listing (whole-table read plus a journal verification per session)
/// and then a batch join — two full reads per page. The single listing
/// now feeds the served page directly; the DTO stays byte-identical to
/// the historical per-handle projection, and the handler performs
/// exactly one store read (one listing statement) at 5k sessions.
#[tokio::test]
async fn session_listing_reads_the_table_once_per_page() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _seed) = test_state(dir.path(), "seed");
    let session = state.deps.session.clone();
    let ws = session.create_workspace("/tmp").unwrap();
    const EXTRA_SESSIONS: usize = 5000;
    let started = std::time::Instant::now();
    for i in 0..EXTRA_SESSIONS {
        session
            .create_session(ws, &format!("batch {i}"), "fake", "m")
            .unwrap();
    }
    let seed_elapsed = started.elapsed();

    // The historical projection, replayed verbatim from the manager
    // listing order: take(cap) then one get_session read per listed
    // handle. This is the golden the single-read DTO must reproduce
    // byte for byte.
    let started = std::time::Instant::now();
    let handles = session.list_sessions(None).unwrap();
    let historical: Vec<serde_json::Value> = handles
        .iter()
        .take(MAX_NATIVE_SESSION_LISTING)
        .map(|h| match h.row() {
            Ok(row) => serde_json::json!({
                "id": row.id.to_string(),
                "title": row.title,
                "provider": row.provider,
                "model": row.model,
                "state": agent_state_tag(row.state),
                "created_ms": row.created_ms,
                "updated_ms": row.updated_ms,
                "listing_revision": row.listing_revision,
            }),
            Err(_) => serde_json::json!({
                "id": h.id().to_string(),
                "title": "",
                "provider": "",
                "model": "",
                "state": "unknown",
            }),
        })
        .collect();
    let historical_elapsed = started.elapsed();
    assert!(handles.len() > EXTRA_SESSIONS);

    // ONE store listing is all the served page needs (the same query
    // the manager listing runs, so the rows and their order are
    // identical).
    let started = std::time::Instant::now();
    let listing = session.store().list_sessions(None).unwrap();
    let single: Vec<serde_json::Value> = listing
        .into_iter()
        .take(MAX_NATIVE_SESSION_LISTING)
        .map(|row| {
            serde_json::json!({
                "id": row.id.to_string(),
                "title": row.title,
                "provider": row.provider,
                "model": row.model,
                "state": agent_state_tag(row.state),
                "created_ms": row.created_ms,
                "updated_ms": row.updated_ms,
                "listing_revision": row.listing_revision,
            })
        })
        .collect();
    let single_projection = started.elapsed();
    assert_eq!(
        single, historical,
        "the single-read projection answers byte-identically to the per-handle read"
    );

    let headers = authed_headers(&state);
    let reads_before = session.store().reader_telemetry().reads;
    let response = super::native_list_sessions(
        State(state.clone()),
        headers,
        Ok(Query(super::NativeSessionListQuery {
            q: None,
            before_revision: None,
            before_id: None,
            before_ms: None,
            snapshot_revision: None,
            snapshot_ms: None,
            limit: None,
        })),
    )
    .await;
    let reads_after = session.store().reader_telemetry().reads;
    let (status, body) = json_body(response).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["sessions"].as_array().expect("sessions array");
    assert_eq!(
        rows, &historical,
        "the served handler answers byte-identically to the per-handle read"
    );
    assert_eq!(
        reads_after - reads_before,
        2,
        "the page reads one bounded snapshot scalar + ONE listing statement"
    );
    eprintln!(
        "session listing: {} sessions (seed {seed_elapsed:?}, manager list+verify {historical_elapsed:?}): single-read projection {single_projection:?}; handler reads delta 1",
        handles.len()
    );
}

/// The History surfaces need a durable time: the listing serves the
/// session row's `created_ms` (additive DTO field; clients parse it
/// tolerantly and never fabricate one).
#[tokio::test]
async fn session_listing_serves_the_durable_creation_time() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "history row");
    let headers = authed_headers(&state);
    let response = super::native_list_sessions(
        State(state.clone()),
        headers,
        Ok(Query(super::NativeSessionListQuery {
            q: None,
            before_revision: None,
            before_id: None,
            before_ms: None,
            snapshot_revision: None,
            snapshot_ms: None,
            limit: None,
        })),
    )
    .await;
    let (status, body) = json_body(response).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["sessions"].as_array().expect("sessions array");
    let row = rows
        .iter()
        .find(|r| r["id"].as_str() == Some(&sid.to_string()))
        .expect("the created session is listed");
    let served = row["created_ms"].as_i64().expect("created_ms is served");
    let durable = state
        .deps
        .session
        .get_session(sid)
        .unwrap()
        .unwrap()
        .row()
        .unwrap()
        .created_ms;
    assert_eq!(
        served, durable,
        "the listing serves the DURABLE creation time"
    );
    assert_eq!(row["title"], "history row");
}

/// The History route pages on the durable `snapshotRevision`, not the
/// wall clock: a session updated after page one (even in the snapshot's
/// millisecond) leaves the rest of this walk and is listed first by the
/// next walk.
#[tokio::test]
async fn session_listing_walk_is_bound_to_the_durable_snapshot_revision() {
    let dir = tempfile::tempdir().unwrap();
    let (state, seed) = test_state(dir.path(), "walk seed");
    let session = state.deps.session.clone();
    let ws = session.create_workspace("/walk").unwrap();
    for i in 0..6 {
        session
            .create_session(ws, &format!("walk {i}"), "fake", "m")
            .unwrap();
    }
    let headers = authed_headers(&state);
    let first = super::native_list_sessions(
        State(state.clone()),
        headers.clone(),
        Ok(Query(super::NativeSessionListQuery {
            q: None,
            before_revision: None,
            before_id: None,
            before_ms: None,
            snapshot_revision: None,
            snapshot_ms: None,
            limit: Some(2),
        })),
    )
    .await;
    let (status, body) = json_body(first).await;
    assert_eq!(status, StatusCode::OK);
    let snapshot_revision = body["snapshotRevision"].as_i64().expect("snapshotRevision");
    let snapshot_ms = body["snapshotMs"].as_i64().expect("snapshotMs");
    assert!(snapshot_revision >= 7);
    let mut seen: Vec<String> = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(seen.len(), 2);

    // The oldest session (last in the walk) is renamed after page one.
    session
        .get_session(seed)
        .unwrap()
        .unwrap()
        .update_session_title("renamed mid-walk")
        .unwrap();
    let bumped = session.get_session(seed).unwrap().unwrap().row().unwrap();
    assert!(bumped.listing_revision > snapshot_revision);

    let mut next = body["nextBefore"].clone();
    let mut pages = 1;
    while !next.is_null() {
        let response = super::native_list_sessions(
            State(state.clone()),
            headers.clone(),
            Ok(Query(super::NativeSessionListQuery {
                q: None,
                before_revision: next["beforeRevision"].as_i64(),
                before_id: next["beforeId"].as_u64(),
                before_ms: next["beforeMs"].as_i64(),
                snapshot_revision: Some(snapshot_revision),
                snapshot_ms: Some(snapshot_ms),
                limit: Some(2),
            })),
        )
        .await;
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::OK);
        seen.extend(
            body["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string()),
        );
        next = body["nextBefore"].clone();
        pages += 1;
        assert!(pages <= 10, "the walk must terminate");
    }
    assert_eq!(
        seen.len(),
        6,
        "the renamed session left this walk: {seen:?}"
    );
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), seen.len(), "no duplicates: {seen:?}");
    assert!(
        !seen.contains(&seed.to_string()),
        "the mid-walk update is not re-served below the cursor"
    );

    let fresh = super::native_list_sessions(
        State(state.clone()),
        headers,
        Ok(Query(super::NativeSessionListQuery {
            q: None,
            before_revision: None,
            before_id: None,
            before_ms: None,
            snapshot_revision: None,
            snapshot_ms: None,
            limit: Some(10),
        })),
    )
    .await;
    let (status, body) = json_body(fresh).await;
    assert_eq!(status, StatusCode::OK);
    let first_id = body["sessions"][0]["id"].as_str().unwrap();
    assert_eq!(
        first_id,
        seed.to_string(),
        "the next walk (fresh snapshot) lists the renamed session first"
    );
    assert!(body["snapshotRevision"].as_i64().unwrap() > snapshot_revision);
}

#[tokio::test]
async fn create_session_accepts_a_registered_provider() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = test_state(dir.path(), "seed");
    let mut headers = authed_headers(&state);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    let response = super::native_create_session(
        State(state.clone()),
        headers,
        Ok(Json(NativeCreateSessionRequest {
            provider: "fake".into(),
            model: "m".into(),
            workspace: Some("/tmp".into()),
            title: Some("accepted".into()),
        })),
    )
    .await;
    let (status, body) = json_body(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["id"].as_str().unwrap_or_default().is_empty());
}

#[tokio::test]
async fn create_session_binds_the_workspace_owner_worktree() {
    // Regression: sessions created over HTTP carried the standalone
    // default worktree id 1. With any earlier worktree row on the daemon,
    // their first shadowed mutating run refused "no registered worktree
    // row"; creation must adopt the workspace's real owner worktree.
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = test_state(dir.path(), "seed");
    let mut headers = authed_headers(&state);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    let workspace = dir.path().join("live-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let response = super::native_create_session(
        State(state.clone()),
        headers,
        Ok(Json(NativeCreateSessionRequest {
            provider: "fake".into(),
            model: "m".into(),
            workspace: Some(workspace.to_string_lossy().into_owned()),
            title: Some("owned".into()),
        })),
    )
    .await;
    let (status, body) = json_body(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sid: u64 = body["id"].as_str().unwrap().parse().unwrap();
    let handle = state
        .deps
        .session
        .get_session(faktor_core::id::SessionId::new(sid))
        .unwrap()
        .unwrap();
    let row = handle.row().unwrap();
    let worktrees = state.deps.session.worktrees_of(row.workspace_id).unwrap();
    assert!(
        worktrees
            .iter()
            .any(|w| w.id as u64 == row.worktree_id.raw()),
        "session {} must own a registered worktree row, got {:?}",
        sid,
        worktrees
    );
}

fn authed_headers(state: &AppState) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", state.deps.auth_token.as_str())
            .parse()
            .unwrap(),
    );
    headers
}

async fn json_body(response: Response) -> (StatusCode, serde_json::Value) {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

async fn abort_request(
    state: &AppState,
    sid: faktor_core::id::SessionId,
    op_id: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let response = native_session_abort(
        State(state.clone()),
        authed_headers(state),
        Path(sid.to_string()),
        Ok(Json(NativeAbortRequest {
            session_id: sid.to_string(),
            op_id: op_id.map(str::to_string),
        })),
    )
    .await;
    json_body(response).await
}

/// A hostile `op_id` that parses as a `u64` but violates the id contract
/// (zero), or never parses (u64 overflow, negative), must be a typed 400
/// naming the field — never `OpId::new(0)` panicking the request task.
/// Valid and absent op ids still round-trip.
#[tokio::test]
async fn native_abort_op_id_validation_never_panics() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = abort_state(dir.path());

    // Valid round-trips: a real queued op id aborts exactly that row,
    // and the absent "abort everything" form answers OK.
    state
        .deps
        .session
        .store()
        .enqueue_prompt(
            sid,
            faktor_core::id::OpId::new(7),
            "q",
            &[],
            None,
            None,
            None,
            1,
        )
        .unwrap();
    let (status, body) = abort_request(&state, sid, Some("7")).await;
    assert_eq!(status, StatusCode::OK, "valid op_id: {body}");
    assert_eq!(body["aborted"], serde_json::json!(["7"]), "{body}");
    let (status, body) = abort_request(&state, sid, None).await;
    assert_eq!(status, StatusCode::OK, "absent op_id: {body}");

    for raw in ["0", "18446744073709551616", "-1", "-9223372036854775808"] {
        let (status, body) = abort_request(&state, sid, Some(raw)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "op_id {raw:?} must be a typed 400: {body}"
        );
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("op_id"),
            "the refusal must name op_id for {raw:?}: {body}"
        );
        assert_eq!(body["error"]["code"], "malformed", "{body}");
    }

    // The session survives the hostile requests: a fresh valid abort
    // still answers OK afterwards.
    state
        .deps
        .session
        .store()
        .enqueue_prompt(
            sid,
            faktor_core::id::OpId::new(8),
            "q",
            &[],
            None,
            None,
            None,
            2,
        )
        .unwrap();
    let (status, body) = abort_request(&state, sid, Some("8")).await;
    assert_eq!(status, StatusCode::OK, "authority stays usable: {body}");
    assert_eq!(body["aborted"], serde_json::json!(["8"]), "{body}");
}

async fn events_request(
    state: &AppState,
    sid: faktor_core::id::SessionId,
    after: Option<u64>,
    limit: Option<u64>,
) -> (StatusCode, serde_json::Value) {
    let response = native_events(
        State(state.clone()),
        authed_headers(state),
        Ok(Query(NativeEventsQuery {
            session: sid.to_string(),
            after,
            limit,
        })),
    )
    .await;
    json_body(response).await
}

async fn messages_request(
    state: &AppState,
    sid: faktor_core::id::SessionId,
    before: Option<i64>,
    limit: Option<u64>,
) -> (StatusCode, serde_json::Value) {
    let response = native_messages(
        State(state.clone()),
        authed_headers(state),
        Ok(Query(NativeMessagesQuery {
            session: sid.to_string(),
            before,
            limit,
        })),
    )
    .await;
    json_body(response).await
}

async fn read_sse(
    response: Response,
    max_chunks: usize,
    timeout: std::time::Duration,
) -> (String, bool) {
    use futures_util::StreamExt as _;
    let mut body = response.into_body().into_data_stream();
    let mut out = String::new();
    let mut ended = false;
    for _ in 0..max_chunks {
        match tokio::time::timeout(timeout, body.next()).await {
            Ok(Some(Ok(chunk))) => out.push_str(&String::from_utf8_lossy(&chunk)),
            Ok(Some(Err(e))) => panic!("SSE body read failed: {e}"),
            Ok(None) => {
                ended = true;
                break;
            }
            Err(_) => break,
        }
    }
    (out, ended)
}

/// Finding 3 (P1): the bound is compared in the `u64` domain before any
/// cast. `u64::MAX` used to wrap to -1 and slip past the comparison;
/// every oversized value is now a typed 400 and every accepted value
/// stays inside the cap.
#[test]
fn native_page_limit_matrix_never_bypasses_the_cap() {
    for max in [MAX_NATIVE_CURSOR_PAGE, MAX_NATIVE_EVENT_PAGE] {
        assert_eq!(page_limit(None, max).unwrap(), max);
        assert_eq!(page_limit(Some(1), max).unwrap(), 1);
        assert_eq!(page_limit(Some(max as u64), max).unwrap(), max);
        for l in [
            0u64,
            max as u64 + 1,
            i64::MAX as u64,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            let e = page_limit(Some(l), max)
                .expect_err(&format!("limit {l} against bound {max} must be refused"));
            assert_eq!(e.http_status, 400, "limit {l}: {e:?}");
            assert_eq!(e.code, "malformed", "limit {l}: {e:?}");
        }
    }
}

/// Finding 3 (P1) over the wire: `/native/events` never accepts a
/// wrapping limit. Every accepted value yields at most the 256-event cap
/// against a 301-event journal; `i64::MAX`, `i64::MAX + 1` and
/// `u64::MAX` are typed 400s, never an oversized or unbounded page.
#[tokio::test]
async fn native_events_limit_matrix_never_bypasses_the_page_cap() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "events-limit");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    for _ in 0..300 {
        handle
            .force_append_event(
                faktor_core::event::EventKind::PhaseChanged,
                faktor_core::state::AgentState::WaitingForModel,
                None,
                None,
            )
            .unwrap();
    }
    let cap = MAX_NATIVE_EVENT_PAGE;
    for (limit, expected) in [(None, cap), (Some(1), 1), (Some(cap as u64), cap)] {
        let (status, body) = events_request(&state, sid, None, limit).await;
        assert_eq!(status, StatusCode::OK, "limit {limit:?}: {body}");
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len() as i64, expected, "limit {limit:?}: {body}");
        assert_eq!(body["hasMore"], serde_json::json!(true), "{body}");
        assert!(
            events.len() as i64 <= cap,
            "a bounded page can never exceed the cap: {body}"
        );
    }
    for limit in [
        Some(0u64),
        Some(cap as u64 + 1),
        Some(i64::MAX as u64),
        Some(i64::MAX as u64 + 1),
        Some(u64::MAX),
    ] {
        let (status, body) = events_request(&state, sid, None, limit).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "limit {limit:?} must be a typed 400: {body}"
        );
        assert_eq!(
            body["error"]["code"], "malformed",
            "limit {limit:?}: {body}"
        );
        assert!(
            body.get("events").is_none(),
            "a refused limit must not carry a page: {body}"
        );
    }
}

/// Finding 3 (P1) over the wire: `/native/messages` is the same matrix
/// against the 200-message cursor cap. A validated limit also keeps the
/// `limit + 1` store read inside the integer domain.
#[tokio::test]
async fn native_messages_limit_matrix_never_bypasses_the_page_cap() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "messages-limit");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    for seq in 1..=300i64 {
        handle
            .put_message(
                seq,
                "user",
                serde_json::json!({ "text": format!("m{seq}") }),
            )
            .unwrap();
    }
    let cap = MAX_NATIVE_CURSOR_PAGE;
    for (limit, expected) in [(None, cap), (Some(1), 1), (Some(cap as u64), cap)] {
        let (status, body) = messages_request(&state, sid, None, limit).await;
        assert_eq!(status, StatusCode::OK, "limit {limit:?}: {body}");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len() as i64, expected, "limit {limit:?}: {body}");
        assert_eq!(body["hasMore"], serde_json::json!(true), "{body}");
        assert!(
            messages.len() as i64 <= cap,
            "a bounded page can never exceed the cap: {body}"
        );
    }
    for limit in [
        Some(0u64),
        Some(cap as u64 + 1),
        Some(i64::MAX as u64),
        Some(i64::MAX as u64 + 1),
        Some(u64::MAX),
    ] {
        let (status, body) = messages_request(&state, sid, None, limit).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "limit {limit:?} must be a typed 400: {body}"
        );
        assert_eq!(
            body["error"]["code"], "malformed",
            "limit {limit:?}: {body}"
        );
        assert!(
            body.get("messages").is_none(),
            "a refused limit must not carry a page: {body}"
        );
    }
}

/// Finding 4: the happy path is unchanged — durable frames stream in
/// journal order with their seq as the frame id, and an exhausted page
/// heartbeats instead of terminating.
#[tokio::test]
async fn native_journal_sse_streams_frames_and_heartbeats() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-happy");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    handle
        .force_append_event(
            faktor_core::event::EventKind::PhaseChanged,
            faktor_core::state::AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
    let response = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (frames, ended) = read_sse(response, 2, std::time::Duration::from_secs(5)).await;
    assert!(!ended, "a healthy journal stream stays open: {frames}");
    assert!(frames.contains("event: phase_changed"), "{frames}");
    assert!(frames.contains("id: 2"), "{frames}");
    assert!(frames.contains("event: heartbeat"), "{frames}");
}

/// Graceful daemon shutdown must END a long-lived stream on the next
/// bounded tick. The readiness flag is the signal: a client that keeps its
/// body open can never hold the listener past the 250 ms poll bound.
#[tokio::test]
async fn native_journal_sse_ends_when_readiness_turns_off() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-shutdown");
    let response = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: Some(0) })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    state
        .ready
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (frames, ended) = read_sse(response, 2, std::time::Duration::from_secs(2)).await;
    assert!(
        ended,
        "a shutdown readiness flag must end the stream: {frames}"
    );
}

/// Finding 4 (P1) hostile: a corrupt durable event must terminate the
/// SSE stream with the typed error frame, never a heartbeat loop over an
/// unreadable authority. The paged twin is loud on the same store.
#[tokio::test]
async fn native_journal_sse_terminates_with_an_error_frame_on_corrupt_journal() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-corrupt");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    handle
        .force_append_event(
            faktor_core::event::EventKind::PhaseChanged,
            faktor_core::state::AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
    state
        .deps
        .session
        .store()
        .sql_execute(&format!(
            "UPDATE event SET state = '\"not_a_state\"' WHERE session_id = {} AND seq = 2",
            sid.raw()
        ))
        .unwrap();
    let (status, body) = events_request(&state, sid, None, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "store_error", "{body}");

    let response = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: None })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (frames, ended) = read_sse(response, 16, std::time::Duration::from_secs(5)).await;
    assert!(
        ended,
        "the stream must terminate on a journal read failure: {frames}"
    );
    assert!(frames.contains("event: error"), "{frames}");
    assert!(
        frames.contains(r#"{"code":"journal_read_failed"}"#),
        "{frames}"
    );
    assert!(
        !frames.contains("heartbeat"),
        "no heartbeat may follow a failed authority read: {frames}"
    );
}

/// Read complete SSE frames off a body stream until `wanted` data
/// frames arrived (heartbeats/comment lines are skipped). Returns
/// `(id, event, data)` triples in wire order and the number of body
/// chunks consumed.
async fn read_wire_frames(
    body: axum::body::Body,
    wanted: usize,
    deadline: std::time::Duration,
) -> (Vec<(u64, String, String)>, usize) {
    use futures_util::StreamExt as _;
    let mut stream = body.into_data_stream();
    let mut pending = Vec::new();
    let mut frames = Vec::new();
    let mut chunks = 0usize;
    while frames.len() < wanted {
        let next = tokio::time::timeout(deadline, stream.next())
            .await
            .expect("SSE read deadline");
        let Some(Ok(chunk)) = next else { break };
        chunks += 1;
        pending.extend_from_slice(&chunk);
        while let Some(end) = pending.windows(2).position(|w| w == b"\n\n") {
            let raw: Vec<u8> = pending.drain(..end + 2).collect();
            let text = String::from_utf8_lossy(&raw);
            let mut id = None;
            let mut event = None;
            let mut data = String::new();
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("id: ") {
                    id = v.trim().parse::<u64>().ok();
                } else if let Some(v) = line.strip_prefix("event: ") {
                    event = Some(v.trim().to_string());
                } else if let Some(v) = line.strip_prefix("data: ") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(v);
                }
            }
            if let (Some(id), Some(event)) = (id, event) {
                if event != "heartbeat" {
                    frames.push((id, event, data));
                }
            }
        }
        // A single coalesced chunk may carry more frames than the
        // consumer reads before disconnecting; model exactly that.
        frames.truncate(wanted);
    }
    (frames, chunks)
}

/// Coalescing is a TRANSPORT concern only: a pre-seeded catch-up page is
/// batched into one HTTP chunk (≤ 64 frames), yet every journal event
/// keeps its own complete frame, id and payload, in journal order.
#[tokio::test]
async fn native_journal_sse_coalesced_chunks_preserve_every_frame_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-coalesce");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    for i in 0..40u64 {
        let kind = if i % 2 == 0 {
            faktor_core::event::EventKind::ModelChunkReceived
        } else {
            faktor_core::event::EventKind::PhaseChanged
        };
        handle
            .force_append_event(
                kind,
                faktor_core::state::AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({"i": i})),
            )
            .unwrap();
    }
    let response = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (frames, chunks) =
        read_wire_frames(response.into_body(), 40, std::time::Duration::from_secs(5)).await;
    assert_eq!(frames.len(), 40, "every durable event must be delivered");
    assert_eq!(
        chunks, 1,
        "a 40-frame catch-up page must coalesce into one HTTP chunk"
    );
    for (index, (id, event, data)) in frames.iter().enumerate() {
        assert_eq!(*id, index as u64 + 2, "journal order is preserved");
        let expected_kind = if index % 2 == 0 {
            "model_chunk_received"
        } else {
            "phase_changed"
        };
        assert_eq!(event, expected_kind);
        let row: serde_json::Value = serde_json::from_str(data).unwrap();
        assert_eq!(row["seq"], serde_json::json!(*id));
        assert_eq!(row["kind"], serde_json::json!(expected_kind));
        assert_eq!(row["payload"]["i"], serde_json::json!(index));
        assert_eq!(row["opId"], serde_json::json!("1"));
    }
}

/// An abrupt disconnect after an arbitrary frame plus a reconnect at the
/// last delivered id yields EXACTLY the events the client missed: no
/// duplicate of the last id, no gap, no dropped final frame (the
/// coalescing buffer is never a cursor authority).
#[tokio::test]
async fn native_journal_sse_reconnect_is_duplicate_and_gap_free() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-reconnect");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    for i in 0..20u64 {
        handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({"i": i})),
            )
            .unwrap();
    }
    let first = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: Some(0) })),
    )
    .await;
    let (head, _) = read_wire_frames(first.into_body(), 7, std::time::Duration::from_secs(5)).await;
    assert_eq!(head.len(), 7);
    let last = head.last().unwrap().0;
    assert_eq!(last, 7, "seq 1 is the durable SessionCreated");

    // More events land while the client is disconnected, including the
    // one the dropped connection would have coalesced.
    for i in 20..25u64 {
        handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({"i": i})),
            )
            .unwrap();
    }
    let second = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: Some(last) })),
    )
    .await;
    // The first connection had already coalesced ALL 20 seeded frames
    // into one chunk; the client consumed 7 of them before dropping the
    // body. Reconnect from id 7 must replay 8..=21 plus the 5 new
    // events: nothing is lost just because the server buffered it.
    let (tail, _) =
        read_wire_frames(second.into_body(), 19, std::time::Duration::from_secs(5)).await;
    assert_eq!(tail.len(), 19, "20 seeded - 7 already seen + 5 new");
    let ids: Vec<u64> = head.iter().chain(tail.iter()).map(|f| f.0).collect();
    assert_eq!(
        ids,
        (1..=26u64).collect::<Vec<_>>(),
        "reconnect resumes strictly above the last delivered id"
    );
    let payloads: Vec<u64> = tail
        .iter()
        .map(|(_, _, data)| {
            serde_json::from_str::<serde_json::Value>(data).unwrap()["payload"]["i"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(payloads, (6..=24u64).collect::<Vec<_>>());
}

/// The emitted chunk bounds are hard: at most 64 data frames and a flush
/// once 64 KiB is reached (a single frame is never split). The byte bound
/// is exercised with ~4 KiB payloads so the flushes happen well below 64
/// frames; the tail remainder must still flush (65..255 partial page).
#[tokio::test]
async fn native_journal_sse_chunk_bounds_never_split_or_reorder_frames() {
    use futures_util::StreamExt as _;
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-bounds");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let pad = "x".repeat(4000);
    for i in 0..200u64 {
        handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({"i": i, "pad": pad})),
            )
            .unwrap();
    }
    let response = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut ids: Vec<u64> = Vec::new();
    let mut chunks = 0usize;
    while ids.len() < 200 {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(3), body.next())
            .await
            .expect("SSE chunk deadline")
            .expect("stream ended before every seeded frame")
            .expect("SSE body read failed");
        chunks += 1;
        let text = String::from_utf8_lossy(&chunk);
        let frame_ids: Vec<u64> = text
            .lines()
            .filter_map(|line| line.strip_prefix("id: "))
            .filter_map(|raw| raw.parse::<u64>().ok())
            .collect();
        assert!(
            frame_ids.len() <= 64,
            "chunk {chunks} carries {} frames (>64)",
            frame_ids.len()
        );
        assert!(
            chunk.len() <= 64 * 1024 + 8 * 1024,
            "chunk {chunks} is {} bytes (64 KiB + one ~4 KiB frame max)",
            chunk.len()
        );
        ids.extend(frame_ids);
    }
    assert!(
        ids == (2..=201u64).collect::<Vec<_>>(),
        "every seeded frame exactly once in journal order"
    );
    assert!(
        chunks >= 4,
        "the 4 KiB payloads must trigger the byte flush, not just the frame cap (got {chunks})"
    );
}

/// A non-delta event flushes the pending delta batch WITH it in one
/// chunk: it never waits behind the delta linger window, and the deltas
/// before/after it stay in journal order.
#[tokio::test]
async fn native_journal_sse_non_delta_flushes_the_pending_delta_batch() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-non-delta");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    for i in 0..10u64 {
        handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({"i": i})),
            )
            .unwrap();
    }
    handle
        .force_append_event(
            faktor_core::event::EventKind::PhaseChanged,
            faktor_core::state::AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
    for i in 10..20u64 {
        handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({"i": i})),
            )
            .unwrap();
    }
    let response = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (first, ended) = read_sse(response, 1, std::time::Duration::from_secs(2)).await;
    assert!(
        !ended,
        "the live stream stays open after the flush: {first}"
    );
    assert_eq!(
        first.matches("event: model_chunk_received").count(),
        20,
        "all 20 pending deltas flush with the non-delta: {first}"
    );
    assert_eq!(
        first.matches("event: phase_changed").count(),
        1,
        "the non-delta is in the same chunk: {first}"
    );
    let phase_at = first.find("event: phase_changed").unwrap();
    let before = first[..phase_at]
        .matches("event: model_chunk_received")
        .count();
    assert_eq!(
        before, 10,
        "the 10 deltas before the non-delta stay ahead of it"
    );
}

/// Regression: a delta-only catch-up page of 65..255 events is emitted as
/// a 64-frame chunk plus a small remainder. With no newer durable tail the
/// remainder must still flush at the coalescing deadline (it is the FINAL
/// data of this connection); it must never be stranded behind the idle
/// heartbeat/fallback loop until another append or a reconnect.
#[tokio::test]
async fn native_journal_sse_partial_delta_page_tail_flushes_without_a_new_append() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "sse-partial-tail");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    for i in 0..100u64 {
        handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::Streaming,
                Some(faktor_core::id::OpId::new(1)),
                Some(serde_json::json!({"i": i})),
            )
            .unwrap();
    }
    let response = native_session_events(
        State(state.clone()),
        authed_headers(&state),
        Path(sid.to_string()),
        // Seq 1 is the durable SessionCreated row; the 100 deltas follow.
        Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The stream stays open after the tail (it is a live subscription), so
    // stop reading as soon as every seeded data frame arrived. A stranded
    // remainder hits the per-read deadline and panics.
    let started = std::time::Instant::now();
    let (frames, chunks) =
        read_wire_frames(response.into_body(), 100, std::time::Duration::from_secs(3)).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the final delta frames must flush at the 8 ms coalescing deadline"
    );
    assert_eq!(frames.len(), 100, "every seeded delta must be delivered");
    let ids: Vec<u64> = frames.iter().map(|f| f.0).collect();
    assert_eq!(ids, (2..=101u64).collect::<Vec<_>>(), "journal order");
    let payloads: Vec<u64> = frames
        .iter()
        .map(|(_, _, data)| {
            serde_json::from_str::<serde_json::Value>(data).unwrap()["payload"]["i"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(payloads, (0..100u64).collect::<Vec<_>>());
    assert!(
        chunks <= 4,
        "the 100-frame tail coalesces into the 64-frame chunk plus one remainder (got {chunks})"
    );
}

// ------------------------------------------------ prompt admission (v27)

const PROMPT_KEY: &str = "b0000000-0000-4000-8000-000000000001";

/// Call the native prompt handler directly with one JSON body and
/// return the status plus the RAW response text (byte-for-byte replay
/// assertions need the exact body, not a re-serialized value).
async fn prompt_call(
    state: &AppState,
    sid: faktor_core::id::SessionId,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let req: NativePromptRequestBody =
        serde_json::from_value(body).expect("test prompt body parses");
    let response = native_prompt(
        State(state.clone()),
        authed_headers(state),
        Path(sid.to_string()),
        Ok(Json(req)),
    )
    .await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn prompt_body(sid: faktor_core::id::SessionId, key: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": sid.to_string(),
        "submission_id": key,
        "prompt": prompt,
    })
}

fn prompt_received_count(handle: &faktor_session::SessionHandle) -> usize {
    handle
        .events_range(1, Some(10_000))
        .unwrap()
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
        .count()
}

/// The REQUIRED prompt submission id is validated with the SAME strict
/// predicate as the task-start field: absent, empty, oversized,
/// uppercase, non-hex and non-ASCII shapes are typed refusals, never a
/// silently generated or dropped key.
#[test]
fn prompt_dto_requires_the_same_canonical_submission_id_shape() {
    let valid = serde_json::json!({
        "session_id": "1",
        "submission_id": "b0000000-0000-4000-8000-0000000000ff",
        "prompt": "hi",
    });
    let req: NativePromptRequestBody =
        serde_json::from_value(valid).expect("a canonical prompt body parses");
    assert_eq!(req.files, None, "absent files normalize to empty");
    // Missing is a typed parse error (never a silent fresh key).
    let err = match serde_json::from_value::<NativePromptRequestBody>(serde_json::json!({
        "session_id": "1", "prompt": "hi",
    })) {
        Ok(_) => panic!("missing submission_id must refuse"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("missing field"), "{err}");
    // A typo of the field name is a 400, never a dropped key.
    let err = match serde_json::from_value::<NativePromptRequestBody>(serde_json::json!({
        "session_id": "1",
        "submissionId": "b0000000-0000-4000-8000-0000000000ff",
        "prompt": "hi",
    })) {
        Ok(_) => panic!("a camelCase typo must refuse"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("unknown field"), "{err}");
    assert!(
        serde_json::from_value::<NativePromptRequestBody>(serde_json::json!({
            "session_id": "1", "submission_id": null, "prompt": "hi",
        }))
        .is_err()
    );
    for hostile in [
        "",
        " ",
        "b0000000-0000-4000-8000-00000000000Z",
        "b0000000-0000-4000-8000-00000000000_",
        "no-uuid-here",
        "☃",
    ] {
        let err = validate_prompt_submission_id(hostile)
            .expect_err("hostile shape must be a typed refusal");
        assert_eq!(err.http_status, 400, "{hostile:?}");
        assert_eq!(err.code, "malformed", "{hostile:?}");
    }
    let oversized =
        "a".repeat(faktor_orchestrator::runtime::task_executor::MAX_SUBMISSION_ID_BYTES + 1);
    assert!(validate_prompt_submission_id(&oversized).is_err());
    assert!(validate_prompt_submission_id(PROMPT_KEY).is_ok());
}

/// Report row 12: a PLAIN prompt carries no billing caps. A cap field
/// (even the 1-micro / 1-token values that once bought a paid call) is
/// an unknown field and is refused at the wire BEFORE any admission or
/// provider call. The zero-cap rule lives on the task-start boundary
/// where caps are valid.
#[test]
fn plain_prompt_refuses_every_billing_cap_before_admission() {
    for cap in [
        serde_json::json!({"max_cost_micro": "1"}),
        serde_json::json!({"max_cost_micro": 1}),
        serde_json::json!({"max_tokens": 1}),
        serde_json::json!({"maxCostMicro": "1"}),
        serde_json::json!({"maxTokens": 1}),
    ] {
        let mut body = prompt_body(
            faktor_core::id::SessionId::new(1),
            "b0000000-0000-4000-8000-0000000000ff",
            "hi",
        );
        let object = body.as_object_mut().unwrap();
        for (key, value) in cap.as_object().unwrap() {
            object.insert(key.clone(), value.clone());
        }
        let err = match serde_json::from_value::<NativePromptRequestBody>(body) {
            Ok(_) => panic!("a billing cap on a plain prompt must refuse: {cap}"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unknown field"), "{cap}: {err}");
    }
}

/// A lost response is retried with the SAME submission id and the SAME
/// body: the replay returns the stored receipt BYTE-FOR-BYTE and creates
/// no second prompt — exactly one `PromptReceived` row, no second user
/// message, no queue row, no admission row beyond the one.
#[tokio::test]
async fn prompt_replay_returns_the_stored_receipt_byte_for_byte_with_one_prompt_row() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "prompt-replay");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let store = state.deps.session.store();
    let body = prompt_body(sid, PROMPT_KEY, "hi");

    let (status, first_text) = prompt_call(&state, sid, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first_text}");
    let first: serde_json::Value = serde_json::from_str(&first_text).unwrap();
    assert!(!first["op_id"].as_str().unwrap().is_empty(), "{first}");
    assert_eq!(first["accepted"], serde_json::json!(true), "{first}");
    let messages_after_first = store.message_count(sid).unwrap();
    let events_after_first = prompt_received_count(&handle);
    let queued_after_first = handle.queued_prompt_count().unwrap();
    assert_eq!(events_after_first, 1, "exactly one prompt was received");

    // The lost-response retry: same key, same body.
    let (status, second_text) = prompt_call(&state, sid, body).await;
    assert_eq!(status, StatusCode::OK, "{second_text}");
    assert_eq!(
        first_text, second_text,
        "the retry must replay the FIRST response byte-for-byte"
    );
    assert_eq!(
        store.message_count(sid).unwrap(),
        messages_after_first,
        "the replay must create no second message row"
    );
    assert_eq!(
        prompt_received_count(&handle),
        events_after_first,
        "the replay must append no second PromptReceived event"
    );
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        queued_after_first,
        "the replay must not enqueue a second prompt"
    );
    // The key carries exactly ONE completed admission storing the exact
    // response bytes (a claim for any other row would answer Fresh/
    // KeyReused, and the byte compare pins the stored receipt).
    let digest = prompt_admission_digest(sid, "hi", &[]);
    assert_eq!(
        store
            .prompt_admission_claim(sid, PROMPT_KEY, &digest, "tx-replay-probe", 0)
            .unwrap(),
        PromptAdmissionClaim::Complete(first_text.clone()),
        "exactly one completed admission storing the response bytes"
    );
}

/// The same key with a DIFFERENT body is a typed 409 conflict with zero
/// mutation: no new prompt event, message or queue row.
#[tokio::test]
async fn prompt_same_key_different_body_is_a_typed_conflict_with_no_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "prompt-key-reused");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let store = state.deps.session.store();

    let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let messages = store.message_count(sid).unwrap();
    let events = prompt_received_count(&handle);

    let (status, text) =
        prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "something else")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "conflict", "{body}");
    assert_eq!(
        store.message_count(sid).unwrap(),
        messages,
        "the refusal must create no message row"
    );
    assert_eq!(
        prompt_received_count(&handle),
        events,
        "the refusal must append no prompt event"
    );
}

/// A key whose claim is still pending with the SAME body is an in-flight
/// 409 BEFORE any `PromptReceived` append or queue/message write: the
/// claim is the first durable act of the prompt path.
#[tokio::test]
async fn prompt_pending_key_with_the_same_body_is_an_in_flight_409_before_any_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "prompt-in-flight");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let store = state.deps.session.store();
    let digest = prompt_admission_digest(sid, "hi", &[]);
    assert_eq!(
        store
            .prompt_admission_claim(
                sid,
                PROMPT_KEY,
                &digest,
                "tx-00000000000000bb",
                handle.now_ms()
            )
            .unwrap(),
        PromptAdmissionClaim::Fresh,
        "the test plants the pending claim an in-flight winner would hold"
    );
    let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "conflict", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("in flight"),
        "{body}"
    );
    assert_eq!(
        prompt_received_count(&handle),
        0,
        "an in-flight refusal must append no prompt event"
    );
    assert_eq!(store.message_count(sid).unwrap(), 0, "no message row");
    assert_eq!(handle.queued_prompt_count().unwrap(), 0, "no queue row");
}

/// A pending key whose stored digest is DIFFERENT from this body is the
/// typed key-reuse conflict (never a lease-long in-flight answer), with
/// zero mutation: no `PromptReceived` append, message or queue row.
#[tokio::test]
async fn prompt_pending_key_with_a_different_body_is_a_typed_key_reuse_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "prompt-pending-key-reused");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let store = state.deps.session.store();
    assert_eq!(
        store
            .prompt_admission_claim(
                sid,
                PROMPT_KEY,
                "digest-from-another-writer",
                "tx-00000000000000bc",
                handle.now_ms()
            )
            .unwrap(),
        PromptAdmissionClaim::Fresh
    );
    let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["code"], "conflict", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("different prompt"),
        "a pending key with different bytes names the reuse, not in-flight: {body}"
    );
    assert_eq!(prompt_received_count(&handle), 0);
    assert_eq!(store.message_count(sid).unwrap(), 0);
    assert_eq!(handle.queued_prompt_count().unwrap(), 0);
}

/// A known pre-admission refusal (an empty prompt) releases the claimed
/// key so the SAME submission may be retried; the refusal itself leaves
/// no prompt event, message or queue row, and the retry admits fresh.
#[tokio::test]
async fn prompt_release_then_retry_admits_and_the_refusal_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "prompt-release-retry");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let store = state.deps.session.store();

    let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "   ")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert_eq!(
        prompt_received_count(&handle),
        0,
        "the pre-admission refusal must append no prompt event"
    );
    assert_eq!(store.message_count(sid).unwrap(), 0, "no message row");
    assert_eq!(handle.queued_prompt_count().unwrap(), 0, "no queue row");

    let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the released key must admit the retry: {text}"
    );
    assert_eq!(
        prompt_received_count(&handle),
        1,
        "the retry admits exactly one prompt"
    );

    // A reuse of the now-completed key with a different body still
    // refuses typed: accepting the retry did not drop the receipt.
    let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "other")).await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
}

// --------------------------------------- admission crash matrix (P1)

/// Reopen the SAME data root as a new daemon boot generation without
/// creating a second workspace/session (the crash matrix retries the
/// original session id).
fn reopen_state(root: &std::path::Path) -> AppState {
    let deps = crate::api::tests::test_deps(root);
    AppState {
        deps: Arc::new(deps),
        auth: Arc::new(std::sync::RwLock::new(None)),
        terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }
}

fn user_message_count(handle: &faktor_session::SessionHandle) -> usize {
    handle
        .messages_page(None, 100)
        .unwrap()
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .count()
}

/// Audit P1 crash matrix, shared harness: arm ONE admission boundary,
/// drive the native prompt handler into the injected crash (the store
/// writer dies and the handler answers typed), drop the "daemon",
/// reopen the SAME data root as a NEW boot generation, run boot
/// admission recovery and assert the pending table is EMPTY.
async fn prompt_crash_and_reopen(
    dir: &tempfile::TempDir,
    point: &'static str,
    ordinal: u64,
) -> (AppState, faktor_core::id::SessionId, serde_json::Value) {
    let (state, sid) = test_state(dir.path(), "prompt-crash");
    let body = prompt_body(sid, PROMPT_KEY, "crash matrix prompt");
    state
        .deps
        .session
        .store()
        .crash_arm(faktor_store::CrashArm { point, ordinal });
    let (status, text) = prompt_call(&state, sid, body.clone()).await;
    assert!(
        status.is_server_error(),
        "seam {point}#{ordinal} must interrupt the prompt: {status} {text}"
    );
    assert!(
        !state.deps.session.store().writer_available(),
        "seam {point}#{ordinal} must kill the durable writer"
    );
    drop(state);
    let state = reopen_state(dir.path());
    let summary = crate::native::recover_pending_admissions(&state.deps.session);
    assert_eq!(summary.failed, 0, "{summary:?}");
    let pending = state
        .deps
        .session
        .store()
        .prompt_admission_pending_page(None, 100)
        .unwrap();
    assert!(
        pending.rows.is_empty(),
        "boot recovery never leaves a pending claim: {summary:?}"
    );
    (state, sid, body)
}

/// Boundary 1: the claim never committed. No row exists; the retry
/// admits exactly one prompt.
#[tokio::test]
async fn prompt_crash_before_claim_retries_as_one_fresh_admission() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid, body) =
        prompt_crash_and_reopen(&dir, "prompt_admission_claim_precommit", 0).await;
    let (status, text) = prompt_call(&state, sid, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    assert_eq!(prompt_received_count(&handle), 1);
    assert_eq!(user_message_count(&handle), 1);
}

/// Boundary 2: the claim committed, no mutation followed. Boot recovery
/// reclaims it; the retry admits exactly one prompt.
#[tokio::test]
async fn prompt_crash_after_claim_before_mutation_reclaims_and_retries_once() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid, body) =
        prompt_crash_and_reopen(&dir, "prompt_admission_claim_committed", 0).await;
    let (status, text) = prompt_call(&state, sid, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    assert_eq!(prompt_received_count(&handle), 1);
    assert_eq!(user_message_count(&handle), 1);
}

/// Boundary 3: the first durable mutation (the `PromptReceived` append)
/// committed before the crash. A bare journal entry is not an accepted
/// prompt (no message, no turn record): recovery reclaims it, repairs
/// the phantom active state and the retry admits exactly one prompt.
#[tokio::test]
async fn prompt_crash_after_first_durable_mutation_reclaims_and_retries_once() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid, body) = prompt_crash_and_reopen(&dir, "ev_committed", 0).await;
    let (status, text) = prompt_call(&state, sid, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    assert_eq!(
        user_message_count(&handle),
        1,
        "exactly one user prompt was materialized"
    );
}

/// Boundary 4: the accepted prompt exists (turn record durable) but the
/// run linkage and the receipt are not. Recovery rebuilds the receipt
/// from the facts and the retry replays it byte-for-byte.
#[tokio::test]
async fn prompt_crash_after_accepted_prompt_before_receipt_replays_from_facts() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "prompt-crash-accepted");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let prompt = "crash matrix prompt";
    let digest = prompt_admission_digest(sid, prompt, &[]);
    let op = state.deps.session.try_next_op_id().unwrap();
    let reservation = format!("tx-{:016x}", op.raw());
    assert!(state
        .deps
        .session
        .store()
        .prompt_admission_claim(sid, PROMPT_KEY, &digest, &reservation, handle.now_ms())
        .unwrap()
        .is_fresh());
    let submitted = handle
        .submit_prompt_with_op_id(prompt, &[], Some(op))
        .unwrap();
    let expected = prompt_receipt_json(&PromptReceipt {
        run_id: reservation,
        op_id: Some(op),
        mode: faktor_orchestrator::runtime::task_executor::TaskRunMode::InSession,
        queued: submitted.queued,
        accepted: true,
    });
    drop(state);

    let state = reopen_state(dir.path());
    let summary = crate::native::recover_pending_admissions(&state.deps.session);
    assert_eq!(summary.replayed, 1, "{summary:?}");
    let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, prompt)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        text, expected,
        "the receipt is rebuilt byte-for-byte from the durable turn facts"
    );
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    assert_eq!(user_message_count(&handle), 1, "no second prompt row");
    assert_eq!(prompt_received_count(&handle), 1);
}

/// Boundary 5: full durable acceptance (prompt, turn, run linkage) before
/// the receipt completion transaction. Boot recovery completes the receipt
/// from the facts; the retry replays it and admits no second prompt.
#[tokio::test]
async fn prompt_crash_after_full_acceptance_before_receipt_completes_from_facts() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid, body) =
        prompt_crash_and_reopen(&dir, "prompt_admission_complete_precommit", 0).await;
    let (status, text) = prompt_call(&state, sid, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    assert_eq!(prompt_received_count(&handle), 1);
    assert_eq!(user_message_count(&handle), 1);
    let receipt: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(receipt["accepted"], serde_json::json!(true), "{text}");
    let run_id = receipt["run_id"].as_str().expect("run id").to_string();
    assert_eq!(
        run_id,
        format!(
            "tx-{:016x}",
            receipt["op_id"]
                .as_str()
                .expect("op id")
                .parse::<u64>()
                .unwrap()
        ),
        "the recovered run id names the reserved turn: {text}"
    );
}

/// Boundary 6: the receipt completed but the owner died before the HTTP
/// response. The retry replays the EXACT stored bytes with zero writes.
#[tokio::test]
async fn prompt_crash_after_receipt_completion_replays_stored_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid, body) =
        prompt_crash_and_reopen(&dir, "prompt_admission_complete_committed", 0).await;
    // The durable receipt is the answer the retry must return exactly.
    let digest = prompt_admission_digest(sid, "crash matrix prompt", &[]);
    let stored = match state
        .deps
        .session
        .store()
        .prompt_admission_claim(sid, PROMPT_KEY, &digest, "tx-probe", 0)
    {
        Ok(PromptAdmissionClaim::Complete(receipt)) => receipt,
        other => panic!("the completed row must replay: {other:?}"),
    };
    let (status, text) = prompt_call(&state, sid, body).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        text, stored,
        "the replay is the stored receipt byte-for-byte"
    );
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    assert_eq!(prompt_received_count(&handle), 1);
    assert_eq!(user_message_count(&handle), 1);
}

/// An unlinkable pending prompt row (no recognizable reservation) is
/// never guessed at: recovery lands the typed conflict and the retry
/// never executes.
#[tokio::test]
async fn prompt_unlinkable_pending_row_lands_typed_conflict_and_never_executes() {
    let dir = tempfile::tempdir().unwrap();
    let (state, sid) = test_state(dir.path(), "prompt-legacy");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    let digest = prompt_admission_digest(sid, "crash matrix prompt", &[]);
    assert!(state
        .deps
        .session
        .store()
        .prompt_admission_claim(
            sid,
            PROMPT_KEY,
            &digest,
            "not-a-reservation",
            handle.now_ms()
        )
        .unwrap()
        .is_fresh());
    drop(state);
    let state = reopen_state(dir.path());
    let summary = crate::native::recover_pending_admissions(&state.deps.session);
    assert_eq!(summary.conflicts, 1, "{summary:?}");
    let (status, text) = prompt_call(
        &state,
        sid,
        prompt_body(sid, PROMPT_KEY, "crash matrix prompt"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{text}");
    assert!(!text.contains("in flight"), "{text}");
    let handle = state.deps.session.get_session(sid).unwrap().unwrap();
    assert_eq!(prompt_received_count(&handle), 0);
    assert_eq!(user_message_count(&handle), 0);
}

// ------------------------------------------- SessionStart lifecycle hook

/// One test AppState over deps whose agent carries an explicit hook
/// registry (the SAME `AgentDeps.hooks` field the production daemon's
/// `FAKTOR_HOOKS` registry occupies).
#[cfg(unix)]
fn hooked_state(
    root: &std::path::Path,
    specs: Vec<faktor_hooks::HookSpec>,
) -> (AppState, Arc<faktor_hooks::HookRegistry>) {
    let hooks = Arc::new(
        faktor_hooks::HookRegistry::try_new().expect("standalone hook registry for tests"),
    );
    for spec in specs {
        hooks.register(spec).unwrap();
    }
    let deps = crate::api::tests::test_deps_with_hooks(root, vec![], Some(hooks.clone()));
    let state = AppState {
        deps: Arc::new(deps),
        auth: Arc::new(std::sync::RwLock::new(None)),
        terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (state, hooks)
}

/// The exact spec shape `FAKTOR_HOOKS=session_start:<script>` produces.
#[cfg(unix)]
fn session_start_spec(
    id: &str,
    script: String,
    deadline_ms: u64,
    failure_policy: faktor_hooks::FailurePolicy,
) -> faktor_hooks::HookSpec {
    faktor_hooks::HookSpec {
        id: id.into(),
        events: vec![faktor_hooks::HookEvent::SessionStart],
        command: "sh".into(),
        args: vec!["-c".into(), script],
        env_allowlist: true,
        deadline_ms,
        failure_policy,
        ..Default::default()
    }
}

#[cfg(unix)]
async fn create_session_call(state: &AppState, title: &str) -> (StatusCode, serde_json::Value) {
    let mut headers = authed_headers(state);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "application/json".parse().unwrap(),
    );
    let response = super::native_create_session(
        State(state.clone()),
        headers,
        Ok(Json(NativeCreateSessionRequest {
            provider: "fake".into(),
            model: "m".into(),
            workspace: Some("/tmp".into()),
            title: Some(title.into()),
        })),
    )
    .await;
    json_body(response).await
}

/// The native `POST /native/session` path fires SessionStart through the
/// agent's hook registry exactly once, after the durable session exists,
/// with the created session id — the SAME ordering as the ACP daemon
/// entry.
#[cfg(unix)]
#[tokio::test]
async fn native_create_session_fires_session_start_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("native-session-start.log");
    let script = format!("echo \"$FAKTOR_HOOK_INPUT\" >> '{}'", marker.display());
    let (state, hooks) = hooked_state(
        dir.path(),
        vec![session_start_spec(
            "env-0",
            script,
            5000,
            faktor_hooks::FailurePolicy::FailClosed,
        )],
    );
    let (status, body) = create_session_call(&state, "hooked").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sid = body["id"].as_str().expect("session id").to_string();
    assert!(
        state
            .deps
            .session
            .get_session(faktor_core::id::SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .is_some(),
        "the hook fired after the durable row existed"
    );
    let log = std::fs::read_to_string(&marker).unwrap();
    let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "the hook runs exactly once: {log}");
    assert!(
        lines[0].contains(&format!("\"session_id\":\"{sid}\"")),
        "the payload names the created session: {log}"
    );
    assert!(lines[0].contains("\"event\":\"session_start\""), "{log}");
    let audit = hooks.audit();
    assert_eq!(audit.len(), 1, "exactly one audit record");
    assert_eq!(audit[0].hook_id, "env-0");
    assert_eq!(audit[0].event, faktor_hooks::HookEvent::SessionStart);
    assert_eq!(audit[0].exit_code, Some(0));
}

/// A failing or hanging SessionStart hook is bounded by its registry
/// deadline and can NEVER fail (or unboundedly stall) session creation:
/// the route answers 200 and the durable row exists in both cases.
#[cfg(unix)]
#[tokio::test]
async fn native_create_session_survives_a_failing_or_hanging_hook() {
    for (id, script, deadline_ms) in [
        ("fail", "exit 3".to_string(), 2000u64),
        ("hang", "sleep 30".to_string(), 250u64),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (state, hooks) = hooked_state(
            dir.path(),
            vec![session_start_spec(
                id,
                script,
                deadline_ms,
                faktor_hooks::FailurePolicy::FailClosed,
            )],
        );
        let t0 = std::time::Instant::now();
        let (status, body) = create_session_call(&state, id).await;
        assert_eq!(status, StatusCode::OK, "{id}: {body}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(8),
            "{id}: the hook deadline must bound session creation"
        );
        let sid = body["id"].as_str().expect("session id").parse().unwrap();
        assert!(
            state
                .deps
                .session
                .get_session(faktor_core::id::SessionId::new(sid))
                .unwrap()
                .is_some(),
            "{id}: the durable session exists"
        );
        let audit = hooks.audit();
        assert_eq!(audit.len(), 1, "{id}: exactly one audit record");
        assert_eq!(
            audit[0].verdict, "deny",
            "{id}: a fail-closed hook outcome is audit-only"
        );
    }
}

/// Perf-wave evidence for the SSE hot path: the borrowed
/// [`NativeEventRowRef`] serializer must emit BYTE-IDENTICAL JSON to the
/// historical `Value` round-trip (`json!` row + `to_string`), for the
/// full field matrix (op id present/absent, payload present (large
/// object + unicode)/absent, several kinds/states). The elapsed probe
/// runs the production 1k-frame replay shape with 2 KiB payloads.
#[test]
fn native_event_row_direct_serializer_is_byte_identical_and_cheaper() {
    let session = faktor_core::id::SessionId::new(42);
    let payload: serde_json::Value = serde_json::json!({
        "kind": "tool_output",
        "text": "x".repeat(2048),
        "unicode": "颜色 π-😀",
        "nested": { "a": [1, 2, 3], "b": null },
    });
    let events: Vec<faktor_core::event::Event> = (0..1000u64)
        .map(|i| {
            faktor_core::event::Event::new(
                faktor_core::id::EventSeq::new(i + 1),
                session,
                if i % 3 == 0 {
                    Some(faktor_core::id::OpId::new(i + 7))
                } else {
                    None
                },
                if i % 2 == 0 {
                    faktor_core::event::EventKind::PhaseChanged
                } else {
                    faktor_core::event::EventKind::PromptReceived
                },
                if i % 5 == 0 {
                    faktor_core::state::AgentState::WaitingForModel
                } else {
                    faktor_core::state::AgentState::Streaming
                },
                1_700_000_000_000 + i as i64,
                if i % 7 == 0 {
                    None
                } else {
                    Some(payload.clone())
                },
            )
        })
        .collect();

    // Independent historical shape: the row exactly as the `json!`
    // path built it (opId a decimal STRING, alphabetical fields).
    let render = |e: &faktor_core::event::Event| -> String {
        serde_json::to_string(&serde_json::json!({
            "kind": e.kind.as_tag(),
            "opId": e.op_id.map(|o| o.to_string()),
            "payload": &e.payload,
            "seq": e.seq.raw(),
            "state": e.state.as_tag(),
            "tsMs": e.ts_ms,
        }))
        .unwrap()
    };
    for e in &events {
        assert_eq!(
            serde_json::to_string(&native_event_row(e)).unwrap(),
            render(e),
            "direct serializer must be byte-identical at seq {}",
            e.seq.raw()
        );
    }

    // 1k frames x 2 KiB payloads (the production catch-up page shape):
    // the historical path deep-cloned the payload into a Value tree and
    // re-serialized the tree; the direct path serializes once.
    let t0 = std::time::Instant::now();
    let mut old_bytes = 0usize;
    for _ in 0..3 {
        for e in &events {
            old_bytes += serde_json::to_string(&native_event_row(e)).unwrap().len();
        }
    }
    let old_elapsed = t0.elapsed();
    let t1 = std::time::Instant::now();
    let mut new_bytes = 0usize;
    let mut row_buf: Vec<u8> = Vec::with_capacity(4096);
    for _ in 0..3 {
        for e in &events {
            row_buf.clear();
            serde_json::to_writer(
                &mut row_buf,
                &NativeEventRowRef {
                    kind: e.kind.as_tag(),
                    op_id: e.op_id.map(OpIdWire),
                    payload: &e.payload,
                    seq: e.seq.raw(),
                    state: e.state.as_tag(),
                    ts_ms: e.ts_ms,
                },
            )
            .unwrap();
            new_bytes += row_buf.len();
        }
    }
    let new_elapsed = t1.elapsed();
    assert_eq!(old_bytes, new_bytes);
    eprintln!(
        "sse frame row (3 x 1000 frames, 2 KiB payloads): value round-trip {old_elapsed:?} -> direct {new_elapsed:?} ({old_bytes} bytes)"
    );
    assert!(
        new_elapsed < old_elapsed,
        "direct serialization must beat the Value round-trip: old {old_elapsed:?} new {new_elapsed:?}"
    );
}
