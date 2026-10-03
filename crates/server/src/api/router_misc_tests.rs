use crate::api::tests::*;
use crate::api::*;

/// A poisoned auth-override lock is authority-bearing: the next auth
/// check must return the typed internal refusal instead of panicking the
/// whole daemon surface.
#[test]
fn poisoned_auth_override_lock_refuses_with_typed_internal_error() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let state = AppState {
        deps: Arc::new(deps),
        auth: Arc::new(std::sync::RwLock::new(None)),
        terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let auth = Arc::clone(&state.auth);
    let poisoner = std::thread::spawn(move || {
        let _guard = auth.write().unwrap();
        panic!("poison the auth override");
    });
    assert!(poisoner.join().is_err());
    assert!(state.auth.is_poisoned());

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer deadbeef".parse().unwrap(),
    );
    let err = crate::native::authed(&headers, &state)
        .expect_err("poisoned auth must refuse, never panic");
    assert_eq!(err.code, "internal");
    assert_eq!(err.http_status, 500);
    assert!(!err.retryable);
    assert!(err.message.contains("poison"), "{}", err.message);
}

/// The terminal-event ring is a strictly derived bounded cache (never an
/// authority): a poisoned ring recovers on the next read instead of
/// killing the endpoint.
#[tokio::test]
async fn poisoned_terminal_event_cache_recovers_on_next_read() {
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let session = deps.session.clone();
    let ws = session.create_workspace("/tmp").unwrap();
    let sid = session
        .create_session(ws, "poisoned-terminal-events", "fake", "m")
        .unwrap()
        .id()
        .to_string();
    let state = AppState {
        deps: Arc::new(deps),
        auth: Arc::new(std::sync::RwLock::new(None)),
        terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let ring = Arc::clone(&state.terminal_events);
    let poisoner = std::thread::spawn(move || {
        let _guard = ring.lock().unwrap();
        panic!("poison the terminal event cache");
    });
    assert!(poisoner.join().is_err());
    assert!(state.terminal_events.is_poisoned());

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", token.as_str()).parse().unwrap(),
    );
    let query: NativeTerminalEventsQuery = serde_json::from_value(serde_json::json!({})).unwrap();
    let response = native_terminal_events(
        axum::extract::State(state.clone()),
        headers,
        axum::extract::Path(sid.clone()),
        Ok(axum::extract::Query(query)),
    )
    .await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::OK,
        "the derived cache must recover under poison"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["sessionId"], sid);
    assert!(body["events"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn native_bootstrap_strict_dtos_and_cursor_stream() {
    let dir = tempfile::tempdir().unwrap();
    let mut deps = test_deps(dir.path());
    // The live-waiter leg must not race the default requester cap under a
    // loaded parallel suite: the route reads `deps.permissions`, so give
    // the test a requester whose only expiry is the durable deadline.
    let permissions = ChannelPermissionRequester::new(std::time::Duration::from_secs(3600));
    deps.permissions = permissions.clone();
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    // Strict create DTO: an unknown field or empty provider is a 400.
    for body in [
        serde_json::json!({"provider": "fake", "model": "m", "smuggled": 1}),
        serde_json::json!({"provider": "", "model": "m"}),
    ] {
        let resp = client
            .post(format!("{base}/native/session"))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{body}");
    }
    // One durable session on the workspace root.
    let resp = client
        .post(format!("{base}/native/session"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "provider": "fake", "model": "m", "workspace": "/tmp", "title": "t-bootstrap",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let created: serde_json::Value = resp.json().await.unwrap();
    let sid = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["title"], "t-bootstrap");
    assert!(created["created_ms"].as_i64().unwrap() > 0);

    // The durable listing carries it.
    let listing: serde_json::Value = client
        .get(format!("{base}/native/sessions"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["id"] == sid && s["title"] == "t-bootstrap"));

    // Prompt strictness: id mismatch, empty prompt, unknown session.
    // Every prompt body carries the REQUIRED submission_id (the durable
    // idempotency key); a missing/empty key is its own typed 400 below.
    let resp = client
        .post(format!("{base}/native/session/{sid}/prompt"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "session_id": "999", "prompt": "x",
            "submission_id": "a0000000-0000-4000-8000-000000000001",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!("{base}/native/session/{sid}/prompt"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "session_id": sid, "prompt": "   ",
            "submission_id": "a0000000-0000-4000-8000-000000000002",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .post(format!("{base}/native/session/999999/prompt"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "session_id": "999999", "prompt": "x",
            "submission_id": "a0000000-0000-4000-8000-000000000003",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .post(format!("{base}/native/session/{sid}/prompt"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"session_id": sid, "prompt": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "a missing submission_id is a typed 400");

    // A real prompt is accepted and lands on the ONE executor entry.
    let resp = client
        .post(format!("{base}/native/session/{sid}/prompt"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "session_id": sid, "prompt": "hi",
            "submission_id": "a0000000-0000-4000-8000-000000000004",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let receipt: serde_json::Value = resp.json().await.unwrap();
    assert!(!receipt["op_id"].as_str().unwrap().is_empty());
    assert!(receipt["accepted"].as_bool().unwrap());

    // Native permission surface: the live set is empty, ids/decisions
    // are strict, and every reply for an id that is NOT a live waiter is
    // a typed 409 — a reply can never plant a decision for a future
    // request.
    let listing: serde_json::Value = client
        .get(format!("{base}/native/permissions?session={sid}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["permissions"], serde_json::json!([]));
    let resp = client
        .get(format!("{base}/native/permissions?session=abc"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = client
        .get(format!("{base}/native/permissions?sessionId={sid}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "unknown query fields stay strict");
    for (body, expect, why) in [
        (
            serde_json::json!({"permission_id": "1", "decision": "maybe"}),
            400,
            "missing session_id",
        ),
        (
            serde_json::json!({"session_id": sid, "permission_id": "0", "decision": "allow"}),
            400,
            "zero permission id",
        ),
        (
            serde_json::json!({"session_id": "abc", "permission_id": "1", "decision": "allow"}),
            400,
            "invalid session id",
        ),
        (
            serde_json::json!({"session_id": sid, "permission_id": "999", "decision": "allow", "x": 1}),
            400,
            "unknown field",
        ),
    ] {
        let resp = client
            .post(format!("{base}/native/permission/reply"))
            .bearer_auth(token.as_str())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), expect, "{why}: {body}");
    }
    // An id that is not a LIVE waiter is refused on every reply — the
    // first reply cannot plant an Allow for a later request.
    let body = serde_json::json!({"session_id": sid, "permission_id": "999", "decision": "allow"});
    let resp = client
        .post(format!("{base}/native/permission/reply"))
        .bearer_auth(token.as_str())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        409,
        "no pre-authorization for a future id: {body}"
    );
    let resp = client
        .post(format!("{base}/native/permission/reply"))
        .bearer_auth(token.as_str())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "still nothing to resolve: {body}");

    // A LIVE waiter: the reply is bound to the owning session. A
    // wrong-session reply is a typed 409 and must NOT consume the waiter;
    // only the owning session's reply resolves it (200 means a real
    // waiter received the decision).
    let sid_num = faktor_core::id::SessionId::new(sid.parse::<u64>().unwrap());
    let permissions_for_waiter = permissions.clone();
    let waiter = tokio::spawn(async move {
        faktor_agent::PermissionRequester::request(
            &*permissions_for_waiter,
            sid_num,
            &faktor_session::ops::PermissionRequest {
                id: 4242,
                op_id: faktor_core::id::OpId::new(1),
                capability: faktor_core::capability::Capability::ExecuteShell {
                    command: "ls".into(),
                },
                event_seq: faktor_core::id::EventSeq::new(1),
                expires_ms: i64::MAX,
            },
        )
        .await
    });
    for _ in 0..200 {
        if !permissions.pending_ids().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(permissions.pending_ids(), vec![4242], "live waiter listed");
    let wrong = serde_json::json!({
        "session_id": "999999", "permission_id": "4242", "decision": "allow"
    });
    let resp = client
        .post(format!("{base}/native/permission/reply"))
        .bearer_auth(token.as_str())
        .json(&wrong)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "{wrong}");
    let error: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        error["error"]["code"], "permission_session_mismatch",
        "wrong-session refusal is typed: {error}"
    );
    assert_eq!(
        permissions.pending_ids(),
        vec![4242],
        "the wrong-session reply does not consume the live waiter"
    );
    let right = serde_json::json!({
        "session_id": sid, "permission_id": "4242", "decision": "allow"
    });
    let resp = client
        .post(format!("{base}/native/permission/reply"))
        .bearer_auth(token.as_str())
        .json(&right)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{right}");
    assert_eq!(
        waiter.await.unwrap().unwrap(),
        faktor_core::capability::PermissionDecision::Allow
    );
    assert!(permissions.pending_ids().is_empty(), "authority drains");

    // The native SSE stream replays the durable journal from cursor 0.
    let resp = client
        .get(format!("{base}/native/session/{sid}/events?after=0"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    use futures_util::StreamExt;
    let mut body = resp.bytes_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), body.next())
        .await
        .expect("stream must not hang")
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains("id: "), "frame carries a cursor id: {text}");
    assert!(
        text.contains("event: "),
        "frame carries an event kind: {text}"
    );
    assert!(text.contains("data: "), "frame carries data: {text}");
    // Unknown query fields stay strict 400s.
    let resp = client
        .get(format!("{base}/native/session/{sid}/events?aftr=1"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_abort_strict_dto_and_targeted_kill() {
    // The native abort is sdk_abort semantics behind the STRICT native
    // DTO (audit 56): any unknown body field — a typo included — is a
    // 400 before anything runs; a valid body kills exactly the targeted
    // queued op and leaves the machine untouched.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/tmp").unwrap();
    let session = manager
        .create_session(ws, "t-abort-native", "fake", "m")
        .unwrap();
    let session_id = session.id().to_string();
    let _ = session.submit_prompt("first", &[]).unwrap();
    let second = session.submit_prompt("second", &[]).unwrap();
    assert!(second.queued, "second prompt must queue behind Preparing");
    let op_id = second.op_id.to_string();

    // Strict DTO rejections: unknown field, realistic typo, missing
    // session_id, unparseable op_id, path/body mismatch, hostile path.
    for evil in [
        format!(r#"{{"session_id":"{session_id}","bogus":1}}"#),
        format!(r#"{{"session_id":"{session_id}","hardBudegt":true}}"#),
    ] {
        let resp = client
            .post(format!("{base}/native/session/{session_id}/abort"))
            .bearer_auth(token.as_str())
            .header("content-type", "application/json")
            .body(evil.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{evil}");
    }
    let resp = client
        .post(format!("{base}/native/session/{session_id}/abort"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"op_id": "1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "missing session_id");
    let resp = client
        .post(format!("{base}/native/session/{session_id}/abort"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"session_id": session_id, "op_id": "not-a-number"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "unparseable op_id");
    let resp = client
        .post(format!("{base}/native/session/999999/abort"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"session_id": session_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "path/body session mismatch");
    let resp = client
        .post(format!("{base}/native/session/abc/abort"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"session_id": session_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "hostile path id");

    // Unauth with a VALID body → 401 (auth gate runs in the handler).
    let resp = client
        .post(format!("{base}/native/session/{session_id}/abort"))
        .json(&serde_json::json!({"session_id": session_id, "op_id": op_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Valid targeted abort of the queued prompt.
    let resp = client
        .post(format!("{base}/native/session/{session_id}/abort"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"session_id": session_id, "op_id": op_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let aborted: serde_json::Value = resp.json().await.unwrap();
    assert!(
        aborted["aborted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o.as_str() == Some(op_id.as_str())),
        "{aborted}"
    );
    assert_eq!(
        session.state().unwrap(),
        faktor_core::state::AgentState::Preparing,
        "a queued-prompt kill must not touch the state machine"
    );
    assert_eq!(session.queued_prompt_count().unwrap(), 0);

    // Unknown session → 404 for a valid body.
    let resp = client
        .post(format!("{base}/native/session/999999/abort"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"session_id": "999999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let _ = handle.request_shutdown();
}

/// The fail-closed default: an embedded host that injects NO terminal
/// policy enforces the `os_isolated`/`Required` contract. On Linux the
/// spawn is genuinely confined by the shared network-namespace backend
/// and the response carries the honest `os_isolated`/`deny_all` profile;
/// on a platform (or host) with no usable backend it is refused typed
/// (403 `execution_denied`) BEFORE any PTY exists, no durable
/// `terminal_*` row is journaled, and the service stays at zero live
/// rows. NEVER an unenforced shell. Reaching an UNISOLATED
/// network-capable terminal requires an explicit grant (the granted test
/// deps, exercised by the other native terminal tests).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_terminal_spawn_is_isolated_or_refused_under_the_fail_closed_default_policy() {
    let dir = tempfile::tempdir().unwrap();
    let mut deps = test_deps(dir.path());
    deps.terminal_policy = TerminalAuthorityPolicy::default();
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let root = dir.path().join("nt-default-root");
    std::fs::create_dir_all(&root).unwrap();
    let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "nt-default-refusal", "fake", "m")
        .unwrap();
    let sid = session.id().to_string();

    let resp = client
        .post(format!("{base}/native/session/{sid}/terminal"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"command": "/bin/sleep", "args": ["30"]}))
        .send()
        .await
        .unwrap();
    match resp.status().as_u16() {
        200 => {
            // Linux with a usable kernel/user-namespace policy: the child
            // really ran inside the sandbox namespace, and the response
            // records exactly that.
            if !cfg!(target_os = "linux") {
                panic!("no platform without a backend may admit an os_isolated spawn");
            }
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["executionProfile"]["shell"], "os_isolated", "{body}");
            assert_eq!(body["executionProfile"]["network"], "required", "{body}");
            assert_eq!(
                body["executionProfile"]["networkIsolation"], "deny_all",
                "the response records the isolation actually applied: {body}"
            );
            let terminal_id = body["terminalId"].as_str().unwrap();
            assert_eq!(
                native_kill_terminal(&client, &base, &token, &sid, terminal_id).await,
                200
            );
        }
        status => {
            // Platform truth (macOS/Windows) or a refused unshare: the
            // typed fail-closed refusal, BEFORE any child exists.
            assert_eq!(status, 403, "the default is fail-closed");
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["code"], "execution_denied", "{body}");
            assert!(
                body["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("sandbox unavailable"),
                "{body}"
            );
        }
    }

    // Journal honesty: a refusal journals nothing; an isolated spawn's
    // rows are exactly the honest os_isolated/deny_all lifecycle. No
    // terminal is left live.
    let session_handle = manager
        .get_session(SessionId::new(sid.parse().unwrap()))
        .unwrap()
        .unwrap();
    for record in session_handle.ledger_terminal_rows(None).unwrap() {
        let profile: serde_json::Value =
            serde_json::from_str(&record.row.execution_profile).unwrap();
        assert_eq!(profile["shell"], "os_isolated", "{profile}");
        assert_eq!(profile["networkIsolation"], "deny_all", "{profile}");
    }
    let service =
        TerminalService::for_manager_with_policy(&manager, TerminalAuthorityPolicy::default());
    assert_eq!(service.live_rows(), 0, "no terminal may be left live");
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_terminal_lists_live_ptys_with_id_pid_alive() {
    // A live PTY on the daemon appears in /native/session/{id}/terminal
    // (the session id is validated, but PTYs have no session binding
    // yet — all daemon PTYs are listed). Platforms that refuse PTY
    // spawns must still serve the empty listing honestly.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/tmp").unwrap();
    let session = manager.create_session(ws, "t-pty", "fake", "m").unwrap();
    let sid = session.id().to_string();

    let Some(created) =
        native_spawn_terminal(&client, &base, &token, &sid, "/bin/sleep", &["30"]).await
    else {
        // Platform refusal (documented): the session terminal view stays
        // empty and honest.
        let resp = client
            .get(format!("{base}/native/session/{sid}/terminal"))
            .bearer_auth(token.as_str())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!([])
        );
        let _ = handle.request_shutdown();
        return;
    };
    let pty_id = created["terminalId"].as_str().unwrap().to_string();
    let pid = created["pid"].as_u64().unwrap_or(0);
    assert!(pid > 0);

    // The live pty lists with its id, pid and aliveness.
    let resp = client
        .get(format!("{base}/native/session/{sid}/terminal"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let entries = body.as_array().unwrap();
    let mine = entries
        .iter()
        .find(|e| e["id"] == pty_id)
        .expect("the live pty must be listed");
    assert_eq!(mine["pid"], pid);
    assert_eq!(mine["alive"], true);

    // The bounded output snapshot reads through the ONE terminal
    // authority; unknown terminals are typed 404s, never fabricated
    // bytes.
    let resp = client
        .get(format!(
            "{base}/native/session/{sid}/terminals/{pty_id}/output"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let out: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(out["ok"], true);
    assert_eq!(out["alive"], true);
    let resp = client
        .get(format!(
            "{base}/native/session/{sid}/terminals/not-a-terminal/output"
        ))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // Killing it is terminal: the durable row stays in the session view
    // with state killed and alive false.
    assert_eq!(
        native_kill_terminal(&client, &base, &token, &sid, &pty_id).await,
        200
    );
    let resp = client
        .get(format!("{base}/native/session/{sid}/terminal"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let mine = body
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == pty_id)
        .expect("the killed terminal row stays durable");
    assert_eq!(mine["alive"], false);
    assert_eq!(mine["state"], "killed");
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_usage_aggregates_budget_and_spent_across_sessions() {
    // /native/usage sums the durable usage facts (kind "usage", keys
    // budget/spent) across sessions; hostile non-numeric values are
    // skipped, sessions without usage facts are counted but not listed.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/tmp").unwrap();
    let s1 = manager.create_session(ws, "t-u1", "fake", "m").unwrap();
    let ws2 = manager.create_workspace("/tmp2").unwrap();
    let s2 = manager.create_session(ws2, "t-u2", "fake", "m").unwrap();
    let ws3 = manager.create_workspace("/tmp3").unwrap();
    let _s3 = manager.create_session(ws3, "t-u3", "fake", "m").unwrap();
    // Real usage facts...
    s1.upsert_memory_fact("usage", "budget", "90000").unwrap();
    s1.upsert_memory_fact("usage", "spent", "1234").unwrap();
    s2.upsert_memory_fact("usage", "budget", "10000").unwrap();
    s2.upsert_memory_fact("usage", "spent", "42").unwrap();
    // ...hostile rows (non-numeric / other kinds / other keys) never
    // break the aggregate.
    s2.upsert_memory_fact("usage", "budget", "not-a-number")
        .unwrap();
    s2.upsert_memory_fact("usage", "rogue", "900000").unwrap();
    s2.upsert_memory_fact("preference", "budget", "700000")
        .unwrap();

    let resp = client
        .get(format!("{base}/native/usage"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .get(format!("{base}/native/usage"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessions"], 3);
    // The later hostile budget upsert REPLACED s2's numeric budget
    // (upsert semantics) with a non-numeric value, which is skipped:
    // totals carry only the numeric facts.
    assert_eq!(body["totals"]["budget"], 90000);
    assert_eq!(body["totals"]["spent"], 1276);
    let per = body["perSession"].as_array().unwrap();
    assert_eq!(per.len(), 2, "fact-less sessions are counted, not listed");
    assert!(per.iter().any(|e| {
        e["sessionId"] == s1.id().to_string() && e["budget"] == 90000 && e["spent"] == 1234
    }));
    assert!(per.iter().any(|e| {
        e["sessionId"] == s2.id().to_string() && e["budget"].is_null() && e["spent"] == 42
    }));
    let _ = handle.request_shutdown();
}

// ------------------------------------ orchestration graph (audit 93)

#[tokio::test]
async fn native_terminal_ownership_scope_isolation_and_lifetime_events() {
    // The terminal unification: terminals spawned under session A carry
    // durable {session_id, task_id, agent_id, operation_id} + terminal
    // UUID ownership; the session-scoped view of B never shows A's
    // terminal even when the daemon owns both, and the lifetime log is
    // the durable row stream (created -> running -> killed), session-
    // scoped and strictly above the cursor.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    // The execution authority resolves the default terminal cwd to the
    // session's candidate root, so this fixture root must be a real
    // directory.
    let root = dir.path().join("nt-root");
    std::fs::create_dir_all(&root).unwrap();
    let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let a = manager.create_session(ws, "t-pty-a", "fake", "m").unwrap();
    let b = manager.create_session(ws, "t-pty-b", "fake", "m").unwrap();
    let a_sid = a.id().to_string();
    let b_sid = b.id().to_string();

    // Hostile/unknown sessions and strict DTO checks first.
    for path in [
        "/native/terminals?session=0",
        "/native/terminals?session=abc",
    ] {
        let resp = native_get(&client, &base, &token, path).await;
        assert_eq!(resp.status(), 400, "{path}");
    }
    let resp = native_get(&client, &base, &token, "/native/terminals?session=999999").await;
    assert_eq!(resp.status(), 404);
    let resp = native_get(&client, &base, &token, "/native/terminals?session=1&limt=2").await;
    assert_eq!(resp.status(), 400, "unknown query field is a 400");

    // Spawn owned terminals under A and B through the ONE durable
    // service. The response carries the terminal UUID plus the legacy
    // pty alias, the durable ownership and the process identity.
    let Some(pty_a) =
        native_spawn_terminal(&client, &base, &token, &a_sid, "/bin/sleep", &["60"]).await
    else {
        // Platform refusal: the session-scoped view stays empty + the
        // documented unowned/note shape still serves.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/terminals?session={a_sid}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["terminals"], serde_json::json!([]));
        assert_eq!(body["unowned"], 0);
        let _ = handle.request_shutdown();
        return;
    };
    let pty_a_id = pty_a["terminalId"].as_str().unwrap().to_string();
    assert_eq!(pty_a["ptyId"], pty_a_id, "legacy alias is the UUID");
    let pid_a = pty_a["pid"].as_u64().unwrap();
    assert!(pid_a > 0);
    assert_eq!(pty_a["state"], "running");
    assert_eq!(pty_a["sessionId"], a_sid);
    assert_eq!(pty_a["taskId"], "1", "standalone session task identity");
    assert!(pty_a["agentId"].is_null());
    assert!(pty_a["startTimeMs"].as_i64().unwrap_or(0) > 0);
    let op_a = pty_a["operationId"].as_str().unwrap().to_string();
    assert!(!op_a.is_empty());
    // The durable effective execution profile rides the spawn response:
    // the cwd is the session's candidate root and the budgets/profiles
    // are the admitted ones (never just the owner).
    let profile = &pty_a["executionProfile"];
    assert!(profile.is_object(), "{pty_a}");
    assert_eq!(profile["sessionId"], a.id().raw());
    assert_eq!(
        profile["cwd"],
        root.canonicalize().unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(profile["filesystem"], "workspace+external:ask-ask");
    assert_eq!(profile["network"], "none");
    // Phase D shell contract (residual closed): these test deps INJECT
    // the explicit user-granted network-capable shell (never presented as
    // OS isolation), which is why this spawn is admitted; the
    // authority's own default — like a default-config daemon — is the
    // fail-closed `os_isolated` shape and either runs the child in the
    // sandbox network namespace (Linux) or refuses typed before spawn
    // (see the default-contract and terminal_authority tests).
    assert_eq!(profile["shell"], "network_capable_user_granted");
    assert!(profile["budgets"]["maxProcesses"].as_u64().unwrap_or(0) > 0);

    let Some(pty_b) =
        native_spawn_terminal(&client, &base, &token, &b_sid, "/bin/sleep", &["60"]).await
    else {
        let _ = native_kill_terminal(&client, &base, &token, &a_sid, &pty_a_id).await;
        let _ = handle.request_shutdown();
        return;
    };
    let pty_b_id = pty_b["terminalId"].as_str().unwrap().to_string();

    // A's scoped view: ONLY A's terminal with full ownership.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/terminals?session={a_sid}"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessionId"], a_sid);
    assert_eq!(body["unowned"], 0);
    assert_eq!(body["note"], "");
    let rows = body["terminals"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{body}");
    assert_eq!(rows[0]["id"], pty_a_id);
    assert_eq!(rows[0]["terminalId"], pty_a_id);
    assert_eq!(rows[0]["pid"], pid_a);
    assert_eq!(rows[0]["alive"], true);
    assert_eq!(rows[0]["sessionId"], a_sid);
    assert_eq!(rows[0]["taskId"], "1");
    assert_eq!(rows[0]["operationId"], op_a);
    assert_eq!(rows[0]["state"], "running");
    assert!(rows[0]["spawnedMs"].as_i64().unwrap_or(0) > 0);
    assert!(rows[0]["agentId"].is_null());

    // B's scoped view NEVER contains A's terminal although the daemon
    // owns both: B sees exactly its own row.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/terminals?session={b_sid}"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    let rows = body["terminals"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "B sees only its own terminal: {body}");
    assert_eq!(rows[0]["terminalId"], pty_b_id);
    assert!(
        rows.iter().all(|r| r["terminalId"] != pty_a_id),
        "A's terminal must never surface in B's view: {body}"
    );

    // Cross-session control is denied typed: B can never drive A's
    // terminal through the IDs it can name.
    let denied = client
        .post(format!(
            "{base}/native/session/{b_sid}/terminals/{pty_a_id}/input"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"data": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404, "foreign scope is denied");
    let denied = client
        .post(format!(
            "{base}/native/session/{b_sid}/terminals/{pty_a_id}/kill"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404, "foreign kill is denied");

    // A's terminal is still alive and untouched.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/terminals?session={a_sid}"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["terminals"][0]["state"], "running");
    assert_eq!(body["terminals"][0]["alive"], true);

    // The legacy daemon-level view lists both (durable rows + the
    // retired compat PTY rows).
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{a_sid}/terminal"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let rows: serde_json::Value = resp.json().await.unwrap();
    let rows = rows.as_array().unwrap();
    let mine = rows
        .iter()
        .find(|r| r["id"] == pty_a_id)
        .expect("A's terminal in the daemon view");
    assert_eq!(mine["sessionId"], a_sid, "ownership annotated: {mine}");
    let other = rows
        .iter()
        .find(|r| r["id"] == pty_b_id)
        .expect("B's terminal in the daemon view");
    assert_eq!(other["sessionId"], b_sid);

    // Session-scoped durable lifetime events: created + running frames
    // with the terminal ownership; B's log never contains A's frames.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{a_sid}/terminal/events"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["sessionId"], a_sid);
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "created + running: {body}");
    assert_eq!(events[0]["type"], "terminal_created");
    assert_eq!(events[1]["type"], "terminal_running");
    assert_eq!(events[0]["terminalId"], pty_a_id);
    assert_eq!(events[0]["ptyId"], pty_a_id);
    assert_eq!(events[0]["pid"], pid_a);
    assert!(events[0]["id"].as_u64().unwrap() > 0);
    assert_eq!(body["hasMore"], false);
    assert!(body["nextCursor"].is_null());
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{b_sid}/terminal/events"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0]["terminalId"], pty_b_id,
        "no cross-session frames: {body}"
    );

    // Kill A through the pty authority: the Killed row is journaled
    // immediately (no lazy sweep needed) and the child tree dies.
    let kill = native_kill_terminal(&client, &base, &token, &a_sid, &pty_a_id).await;
    assert_eq!(kill, 200);
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{a_sid}/terminal/events"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 3, "created + running + killed: {body}");
    assert_eq!(events[2]["type"], "terminal_killed");
    assert_eq!(events[2]["terminalId"], pty_a_id);
    assert_eq!(events[2]["pid"], pid_a, "kill event keeps the spawn pid");
    assert_eq!(
        body["hasMore"], false,
        "the whole log fits one page: {body}"
    );
    assert!(body["nextCursor"].is_null());
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/terminals?session={a_sid}"),
    )
    .await;
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["terminals"][0]["state"], "killed", "{body}");
    assert_eq!(body["terminals"][0]["alive"], false, "{body}");

    // Input on a terminal row is a typed state refusal (409).
    let refused = client
        .post(format!(
            "{base}/native/session/{a_sid}/terminals/{pty_a_id}/input"
        ))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"data": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 409, "killed terminal refuses I/O");

    // Bounds + hostile ids + strict DTO on the event log and spawn.
    for path in [
        format!("/native/session/{a_sid}/terminal/events?limit=0"),
        format!("/native/session/{a_sid}/terminal/events?limit=201"),
        format!("/native/session/{a_sid}/terminal/events?limt=5"),
        "/native/session/abc/terminal/events".to_string(),
        "/native/session/0/terminal/events".to_string(),
    ] {
        let resp = native_get(&client, &base, &token, &path).await;
        assert_eq!(resp.status(), 400, "{path}");
    }
    let resp = native_get(
        &client,
        &base,
        &token,
        "/native/session/999999/terminal/events",
    )
    .await;
    assert_eq!(resp.status(), 404);
    // Unknown spawn session / hostile body / unknown body field.
    let resp = client
        .post(format!("{base}/native/session/999999/terminal"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"command": "/bin/sleep", "args": ["1"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .post(format!("{base}/native/session/{a_sid}/terminal"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"command": "/bin/sleep", "bogus": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "unknown body field is a 400");
    let resp = client
        .post(format!("{base}/native/session/{a_sid}/terminal"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"args": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "missing command");
    let resp = client
        .post(format!("{base}/native/session/{a_sid}/terminal"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"command": "x".repeat(5000)}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "oversized command rejected");
    let resp = client
        .post(format!("{base}/native/session/{a_sid}/terminal"))
        .json(&serde_json::json!({"command": "/bin/sleep", "args": ["1"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // Cleanup: kill B's terminal so no test child outlives the test.
    assert_eq!(
        native_kill_terminal(&client, &base, &token, &b_sid, &pty_b_id).await,
        200
    );
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_usage_reads_durable_rows_exactly_and_survives_reopen() {
    // P0-63: the usage endpoints read the DURABLE rows — provider_call
    // token columns (incl. prefix observations) and the per-task
    // cost_reservation rows with their route decisions. Numbers match
    // the stored rows exactly, sessions stay isolated, and a reopened
    // store (brand-new manager + server over the same data root) serves
    // the identical JSON.
    let dir = tempfile::tempdir().unwrap();
    let (expected_a, expected_global, sid_a) = {
        let deps = test_deps(dir.path());
        let token = deps.auth_token.clone();
        let manager = deps.session.clone();
        let handle = serve(deps, 0).await.unwrap();
        let client = reqwest::Client::new();
        let base = format!("http://{}", handle.addr);
        let ws_a = manager.create_workspace("/usage-a").unwrap();
        let ws_b = manager.create_workspace("/usage-b").unwrap();
        let a = manager
            .create_session(ws_a, "t-usage-a", "fake", "m")
            .unwrap();
        let b = manager
            .create_session(ws_b, "t-usage-b", "fake", "m")
            .unwrap();
        let ha = manager.get_session(a.id()).unwrap().unwrap();
        let hb = manager.get_session(b.id()).unwrap().unwrap();
        // Typed task rows: A owns task 1 (capped) and an untouched task
        // 2 (null-safe pre-first-reservation view); B owns task 1.
        seed_typed_task(&ha, 1, Some(5000), Some(3), "usage-a");
        seed_typed_task(&ha, 2, Some(1000), None, "usage-a-extra");
        seed_typed_task(&hb, 1, Some(100), None, "usage-b");
        let store = manager.store();
        let ta1 = faktor_core::id::TaskId::new(1);
        let tb1 = faktor_core::id::TaskId::new(1);
        store
            .cost_task_cap_set(a.id(), ta1, Some(1_000_000))
            .unwrap();
        store.cost_task_cap_set(b.id(), tb1, Some(50_000)).unwrap();
        let now = manager.now_ms();
        // A's provider calls: two completed with prefix observations
        // (cacheable-prefix token columns) + one failed row (NULL
        // counters never count).
        let op1 = manager.try_next_op_id().unwrap();
        let op2 = manager.try_next_op_id().unwrap();
        let op3 = manager.try_next_op_id().unwrap();
        ha.settle_usage_with_prefix(
            op1,
            "fake",
            "m",
            "completed",
            Some(900),
            Some(100),
            None,
            Some([7u8; 32]),
            Some(400),
        )
        .unwrap();
        ha.settle_usage_with_prefix(
            op2,
            "fake",
            "m",
            "completed",
            Some(500),
            Some(50),
            None,
            Some([9u8; 32]),
            Some(460),
        )
        .unwrap();
        ha.record_provider_call(op3, "fake", "m", "failed", None, None, Some("boom"))
            .unwrap();
        // B's call: completed WITHOUT a prefix observation.
        let opb = manager.try_next_op_id().unwrap();
        hb.settle_usage_with_prefix(
            opb,
            "fake",
            "m",
            "completed",
            Some(7),
            Some(3),
            None,
            None,
            None,
        )
        .unwrap();
        // A's reservations: one settled (with route JSON), one refunded,
        // one left open.
        let faktor_store::CostReserveOutcome::Granted(r1) =
            store.cost_reserve(a.id(), ta1, op1, 5000, now).unwrap()
        else {
            panic!("reserve r1 must be granted");
        };
        store
                .cost_settle(
                    r1,
                    1000,
                    Some(1000),
                    Some(990),
                    Some(
                        r#"{"provider":"fake","model":"m","estimated_cost_micro":90,"estimated_latency_ms":1,"reasoning":"passthrough","considered":1,"source":"configured"}"#,
                    ),
                    now + 1,
                )
                .unwrap();
        let faktor_store::CostReserveOutcome::Granted(r2) =
            store.cost_reserve(a.id(), ta1, op2, 3000, now).unwrap()
        else {
            panic!("reserve r2 must be granted");
        };
        store.cost_refund(r2, now + 2).unwrap();
        let faktor_store::CostReserveOutcome::Granted(_r3) = store
            .cost_reserve(a.id(), ta1, manager.try_next_op_id().unwrap(), 200, now)
            .unwrap()
        else {
            panic!("reserve r3 must be granted");
        };
        // B's reservation: one settled with NO provider report.
        let faktor_store::CostReserveOutcome::Granted(rb) =
            store.cost_reserve(b.id(), tb1, opb, 4000, now).unwrap()
        else {
            panic!("reserve rb must be granted");
        };
        store
            .cost_settle(rb, 40, Some(40), None, None, now + 1)
            .unwrap();

        // ---- per-session authoritative usage of A
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/usage", a.id()),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let ua: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ua["sessionId"], a.id().to_string());
        // provider-call tokens exactly the stored input+output columns.
        assert_eq!(ua["providerCalls"]["tokens"], 1550);
        // prefix observations mirror the store rows.
        let prefix = store.provider_call_prefix_rows(a.id()).unwrap();
        let observed = ua["providerCalls"]["prefixObservations"]
            .as_array()
            .unwrap();
        assert_eq!(observed.len(), prefix.len());
        for (row, o) in prefix.iter().zip(observed) {
            assert_eq!(o["rowId"], row.row_id);
            assert_eq!(o["promptTokens"], row.prompt_tokens as u64);
            assert_eq!(
                o["stability"],
                row.prefix_stability
                    .map(|v| serde_json::json!(v))
                    .unwrap_or(serde_json::Value::Null)
            );
        }
        // The stability aggregate equals the store's aggregate (f64
        // equality through the JSON wire is checked within one ulp: the
        // client-side serde_json parser is not the round-trip-exact
        // parser, so a long decimal can land one ulp off the stored
        // double; the endpoint itself serves the store's exact value).
        let agg = store
            .session_stored_prefix_stability(a.id())
            .unwrap()
            .unwrap();
        let ps = &ua["prefixStability"];
        assert_eq!(ps["observations"], agg.observations);
        let near =
            |x: f64, y: f64| (x - y).abs() <= 2.0 * f64::EPSILON * x.abs().max(y.abs()).max(1.0);
        assert!(
            near(ps["mean"].as_f64().unwrap(), agg.mean),
            "mean differs by more than 1 ulp: {:?} vs {:?}",
            ps["mean"],
            agg.mean
        );
        assert!(
            near(ps["stdDev"].as_f64().unwrap(), agg.std_dev),
            "stdDev differs: {:?} vs {:?}",
            ps["stdDev"],
            agg.std_dev
        );
        // Task entries: durable budget envelope + reservation rows.
        let tasks = ua["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2, "{ua}");
        let t1 = &tasks[0];
        assert_eq!(t1["taskId"], "1");
        assert_eq!(t1["budget"]["maxTokens"], 5000);
        assert_eq!(t1["budget"]["maxTurns"], 3);
        assert_eq!(t1["budget"]["spentTokens"], 0);
        assert_eq!(money(&t1["budget"]["spentCostMicro"]), 1000);
        assert_eq!(money(&t1["budget"]["maxCostMicro"]), 1_000_000);
        assert_eq!(money(&t1["budget"]["openReservedMicro"]), 200);
        let res = &t1["reservations"];
        assert_eq!(res["open"]["count"], 1);
        assert_eq!(money(&res["open"]["predictedMicro"]), 200);
        assert_eq!(res["settled"]["count"], 1);
        assert_eq!(money(&res["settled"]["predictedMicro"]), 5000);
        assert_eq!(money(&res["settled"]["spentMicro"]), 1000);
        assert_eq!(money(&res["settled"]["providerReportedMicro"]), 990);
        assert_eq!(res["refunded"]["count"], 1);
        assert_eq!(money(&res["refunded"]["predictedMicro"]), 3000);
        assert_eq!(res["uncertain"]["count"], 0);
        let routes = res["routeDecisions"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["reservationId"], r1);
        assert_eq!(money(&routes[0]["spentMicro"]), 1000);
        assert_eq!(routes[0]["decision"]["provider"], "fake");
        assert_eq!(money(&routes[0]["decision"]["estimated_cost_micro"]), 90);
        // Untouched task 2: null-safe pre-first-reservation budget.
        let t2 = &tasks[1];
        assert_eq!(t2["taskId"], "2");
        assert_eq!(t2["budget"]["maxTokens"], 1000);
        assert_eq!(t2["budget"]["maxCostMicro"], serde_json::Value::Null);
        assert_eq!(money(&t2["budget"]["spentCostMicro"]), 0);
        assert_eq!(money(&t2["budget"]["openReservedMicro"]), 0);
        assert_eq!(t2["reservations"]["settled"]["count"], 0);

        // ---- isolation: B's usage never carries A's rows.
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/usage", b.id()),
        )
        .await;
        let ub: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(ub["providerCalls"]["tokens"], 10, "B only sees its call");
        assert_eq!(
            ub["providerCalls"]["prefixObservations"],
            serde_json::json!([]),
            "B recorded no prefix"
        );
        assert_eq!(ub["tasks"][0]["taskId"], "1");
        assert_eq!(money(&ub["tasks"][0]["budget"]["spentCostMicro"]), 40);
        assert_eq!(ub["tasks"][0]["reservations"]["settled"]["count"], 1);
        assert_eq!(
            money(&ub["tasks"][0]["reservations"]["settled"]["spentMicro"]),
            40
        );
        assert_eq!(ub["tasks"].as_array().unwrap().len(), 1);

        // ---- global aggregate: durable numbers over every session.
        let resp = native_get(&client, &base, &token, "/native/usage").await;
        assert_eq!(resp.status(), 200);
        let gu: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(gu["sessions"], 2);
        assert_eq!(gu["durable"]["providerCalls"]["tokens"], 1560);
        assert_eq!(gu["durable"]["providerCalls"]["prefixObservations"], 2);
        assert_eq!(gu["durable"]["providerCalls"]["prefixTokens"], 860);
        assert_eq!(
            gu["durable"]["providerCalls"]["prefixStabilityObservations"],
            2
        );
        assert_eq!(money(&gu["durable"]["taskSpend"]["settledCostMicro"]), 1040);
        let res = &gu["durable"]["reservations"];
        assert_eq!(res["settled"]["count"], 2);
        assert_eq!(money(&res["settled"]["spentMicro"]), 1040);
        assert_eq!(money(&res["settled"]["providerReportedMicro"]), 990);
        assert_eq!(res["refunded"]["count"], 1);
        assert_eq!(money(&res["refunded"]["predictedMicro"]), 3000);
        assert_eq!(res["open"]["count"], 1);
        assert_eq!(money(&res["open"]["predictedMicro"]), 200);
        assert_eq!(res["uncertain"]["count"], 0);

        // ---- crash-recovery semantics (schema v17+): a crash closes
        // every surviving in-flight reservation split on the durable
        // dispatch marker — this one never left the process
        // (`reserved`, marker NULL), so recovery REFUNDS it (never
        // spent, its prediction released); only dispatched-marker rows
        // go UNCERTAIN. The aggregate follows.
        store.cost_abandon_open_reservations(now + 5).unwrap();
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/native/session/{}/usage", a.id()),
        )
        .await;
        let ua_after: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            money(&ua_after["tasks"][0]["budget"]["openReservedMicro"]),
            0
        );
        assert_eq!(
            money(&ua_after["tasks"][0]["budget"]["spentCostMicro"]),
            1000
        );
        assert_eq!(ua_after["tasks"][0]["reservations"]["open"]["count"], 0);
        assert_eq!(
            ua_after["tasks"][0]["reservations"]["uncertain"]["count"],
            0
        );
        assert_eq!(ua_after["tasks"][0]["reservations"]["refunded"]["count"], 2);
        assert_eq!(
            money(&ua_after["tasks"][0]["reservations"]["refunded"]["predictedMicro"]),
            3200
        );
        let resp = native_get(&client, &base, &token, "/native/usage").await;
        let gu_after: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(gu_after["durable"]["reservations"]["refunded"]["count"], 2);
        assert_eq!(gu_after["durable"]["reservations"]["uncertain"]["count"], 0);

        // Capture the authoritative snapshots for the reopen check.
        let expected_a = ua_after;
        let expected_global = gu_after;
        let _ = handle.request_shutdown();
        (expected_a, expected_global, a.id())
    };
    // ---- reopen durability: a brand-new manager (and server) over the
    // same data root serves the IDENTICAL usage JSON.
    let deps2 = test_deps(dir.path());
    let token2 = deps2.auth_token.clone();
    let handle2 = serve(deps2, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base2 = format!("http://{}", handle2.addr);
    let resp = native_get(
        &client,
        &base2,
        &token2,
        &format!("/native/session/{sid_a}/usage"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let reopened: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(reopened, expected_a, "usage survives a reopen exactly");
    let resp = native_get(&client, &base2, &token2, "/native/usage").await;
    let reopened_global: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(reopened_global, expected_global);
    let _ = handle2.request_shutdown();
}

#[tokio::test]
async fn native_usage_real_turn_with_cache_and_reasoning_tokens() {
    // P0-63 end-to-end: a REAL turn through the wire surface against a
    // provider whose usage frame carries only cache reads/writes +
    // reasoning tokens and a provider-reported cost. The runtime
    // settlement persists the folded totals and the reservation rows;
    // /native/session/{id}/usage then reports EXACTLY the stored rows.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps_full(dir.path(), vec![Arc::new(CacheUsageProvider)]);
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let tasks = deps.tasks.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let ws = manager.create_workspace("/usage-e2e").unwrap();
    let s = manager
        .create_session(ws, "t-usage-e2e", "fake", "m")
        .unwrap();
    let sid = s.id().to_string();
    seed_typed_task(&s, 1, None, None, "usage-e2e");
    // The session row task identity drives the reserve (task 1 row).
    assert_eq!(s.task_id().unwrap().raw(), 1);

    // Drive the turn through the ONE executor entry ordinary prompts
    // use (the same edge the HTTP handler reaches).
    let service = crate::native::PromptExecutionService::new(tasks, manager.clone());
    service
        .prompt(
            s.id(),
            crate::native::PromptRequest {
                prompt: "hi".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut body = serde_json::Value::Null;
    for _ in 0..300 {
        let resp = native_get(
            &client,
            &base,
            &token,
            &format!("/session/{sid}/projection"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        body = resp.json().await.unwrap();
        if body["state"]["machine"] == "ready_for_next_turn" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(body["state"]["machine"], "ready_for_next_turn", "{body}");

    // The stored rows: folded cache/reasoning totals, prefix observation,
    // one settled reservation with the reported cost.
    let store = manager.store();
    let tokens = store.session_usage_tokens(s.id()).unwrap();
    assert_eq!(
        tokens, 12,
        "cache reads 7 + writes 2 + reasoning 3 (in=9,out=3)"
    );
    let prefix = store.provider_call_prefix_rows(s.id()).unwrap();
    assert_eq!(
        prefix.len(),
        1,
        "the completed call recorded its prefix: {prefix:?}"
    );
    let cost = store
        .cost_task_row(s.id(), faktor_core::id::TaskId::new(1))
        .unwrap()
        .unwrap();
    assert_eq!(cost.spent_cost_micro, 123, "provider-reported cost wins");
    let reservations = store
        .cost_reservations_of(s.id(), faktor_core::id::TaskId::new(1), 10)
        .unwrap();
    assert_eq!(reservations.len(), 1);
    assert_eq!(reservations[0].status, "settled");
    // The passthrough test policy consulted NO pricing authority (its
    // decision snapshot is None), so there is no honest locally
    // calculated amount: the provider-reported cost is the ONLY amount
    // and wins both the v18 canonical columns (`settled_cost_micro`,
    // `provider_reported_cost_micro`) and the folded task spend. The
    // pre-B2 "tokens x 1 microUSD local estimate" was abolished — a
    // fabricated number never lands next to a real report.
    assert_eq!(reservations[0].provider_reported_cost_micro, Some(123));
    assert_eq!(reservations[0].provider_reported_micro, Some(123));
    assert_eq!(reservations[0].settled_cost_micro, Some(123));
    assert_eq!(reservations[0].provider_cost_micro, None);
    assert_eq!(
        reservations[0].cost_basis.as_deref(),
        Some(faktor_store::COST_BASIS_PROVIDER_REPORTED)
    );

    // The endpoint reports exactly the stored rows.
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{sid}/usage"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let u: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(u["providerCalls"]["tokens"], tokens);
    let obs = u["providerCalls"]["prefixObservations"].as_array().unwrap();
    assert_eq!(obs.len(), prefix.len());
    assert_eq!(obs[0]["promptTokens"], prefix[0].prompt_tokens as u64);
    assert_eq!(
        obs[0]["stability"],
        prefix[0]
            .prefix_stability
            .map(|v| serde_json::json!(v))
            .unwrap_or(serde_json::Value::Null)
    );
    let tasks = u["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        money(&tasks[0]["budget"]["spentCostMicro"]),
        cost.spent_cost_micro
    );
    assert_eq!(tasks[0]["budget"]["maxCostMicro"], serde_json::Value::Null);
    assert_eq!(money(&tasks[0]["budget"]["openReservedMicro"]), 0);
    let res = &tasks[0]["reservations"];
    assert_eq!(res["settled"]["count"], 1);
    assert_eq!(
        money(&res["settled"]["spentMicro"]),
        123,
        "the folded actual (provider-reported) is what was spent"
    );
    assert_eq!(money(&res["settled"]["providerReportedMicro"]), 123);
    let routes = res["routeDecisions"].as_array().unwrap();
    assert!(!routes.is_empty(), "the routed call records its decision");
    assert_eq!(money(&routes[0]["providerReportedMicro"]), 123);
    assert_eq!(money(&routes[0]["spentMicro"]), 123);
    assert!(
        routes[0]["decision"]["provider"] == "fake" || routes[0]["decision"].is_object(),
        "{routes:?}"
    );

    // Usage of a never-used sibling session is empty, never A's rows.
    let ws2 = manager.create_workspace("/usage-e2e-b").unwrap();
    let b = manager
        .create_session(ws2, "t-usage-e2e-b", "fake", "m")
        .unwrap();
    let resp = native_get(
        &client,
        &base,
        &token,
        &format!("/native/session/{}/usage", b.id()),
    )
    .await;
    let ub: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ub["providerCalls"]["tokens"], 0);
    assert_eq!(ub["tasks"], serde_json::json!([]));
    let _ = handle.request_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_plan_global_ownership_converts_once_at_the_native_dto_boundary() {
    // A legacy client posts ONE plan-global ownership with two mutating
    // items that carry none of their own: the DTO boundary converts it
    // ONCE onto the items (the runtime never sees the plan-global
    // value), and the run is accepted with explicit per-item ownership
    // on the durable assignment rows.
    use faktor_orchestrator::runtime::OrchestratorRuntime;
    let dir = tempfile::tempdir().unwrap();
    let rig = native_task_rig(dir.path(), vec![], false, false);
    seed_native_owner(&rig.owner_root);
    let NativeTaskRig {
        deps,
        manager,
        parent: sid,
        ..
    } = rig;
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);

    let resp = client
        .post(format!("{base}/native/session/{sid}/task-runs"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({
            "goal": "legacy ownership",
            "submission_id": "99999999-9999-4999-8999-999999999999",
            "ownership": "IsolatedWorktree",
            "work_items": [
                {"id": "a", "kind": "Implementation"},
                {"id": "b", "kind": "Implementation", "depends_on": ["a"]},
            ],
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let start: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{start}");
    let run_id = start["run_id"].as_str().unwrap().to_string();
    let mut assignments =
        OrchestratorRuntime::assignment_rows(manager.clone(), sid, &run_id).unwrap();
    for _ in 0..400 {
        if !assignments.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        assignments = OrchestratorRuntime::assignment_rows(manager.clone(), sid, &run_id).unwrap();
    }
    assert_eq!(assignments.len(), 2);
    for a in &assignments {
        assert_eq!(
            a.ownership,
            faktor_core::state::OwnershipSpec::IsolatedWorktree,
            "the legacy plan-global value converted onto item {}",
            a.item_id
        );
    }
    for _ in 0..600 {
        let rows = OrchestratorRuntime::registry_rows(manager.clone(), sid, &run_id).unwrap();
        if rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_evidence_foreign_scope_is_typed_denial() {
    // Knowing a valid evidence id is not authorization (audit 82): the
    // same id read under a foreign session's scope is a typed 403.
    let dir = tempfile::tempdir().unwrap();
    let mut deps = test_deps(dir.path());
    let ws = deps.session.create_workspace("/tmp").unwrap();
    let owner = deps
        .session
        .create_session(ws, "owner", "fake", "m")
        .unwrap();
    let foreign = deps
        .session
        .create_session(ws, "foreign", "fake", "m")
        .unwrap();
    let owner_row = owner.row().unwrap();
    let foreign_row = foreign.row().unwrap();
    deps.evidence = Some(evidence_handle(vec![evidence_envelope(
        7,
        owner_row.id.raw(),
        owner_row.workspace_id.raw(),
        true,
    )]));
    let pw = deps.server_password.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    // The owning session reads its own evidence.
    let resp = client
        .get(format!(
            "{base}/native/evidence/7?session={}",
            owner_row.id.raw()
        ))
        .header("x-faktor-server-password", pw.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["id"], 7);
    assert_eq!(body["sessionId"], owner_row.id.raw());
    assert_eq!(body["backingRetained"], true);
    // A foreign session that knows the id gets no bytes and no oracle:
    // 403 with the typed denial code.
    let resp = client
        .get(format!(
            "{base}/native/evidence/7?session={}",
            foreign_row.id.raw()
        ))
        .header("x-faktor-server-password", pw.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "foreign scope must be denied");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "evidence_access_denied");
    // The retrieve path enforces the same scope.
    let resp = client
        .post(format!(
            "{base}/native/evidence/7/retrieve?session={}",
            foreign_row.id.raw()
        ))
        .header("x-faktor-server-password", pw.as_str())
        .json(&serde_json::json!({"selector": "all"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "evidence_access_denied");
    let _ = handle.request_shutdown();
}

#[tokio::test]
async fn native_evidence_unknown_and_oversized_are_honest_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut deps = test_deps(dir.path());
    let ws = deps.session.create_workspace("/tmp").unwrap();
    let session = deps.session.create_session(ws, "t", "fake", "m").unwrap();
    let row = session.row().unwrap();
    deps.evidence = Some(evidence_handle(vec![evidence_envelope(
        7,
        row.id.raw(),
        row.workspace_id.raw(),
        true,
    )]));
    let pw = deps.server_password.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    // Unknown id: 404, never a scope oracle.
    let resp = client
        .get(format!(
            "{base}/native/evidence/999?session={}",
            row.id.raw()
        ))
        .header("x-faktor-server-password", pw.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    // Oversized search selector: the bound is enforced before the store.
    let oversized = "x".repeat(5000);
    let resp = client
        .post(format!(
            "{base}/native/evidence/7/retrieve?session={}",
            row.id.raw()
        ))
        .header("x-faktor-server-password", pw.as_str())
        .json(&serde_json::json!({
            "selector": "search",
            "query": oversized,
            "max_hits": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "oversized selector is a loud 400");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "evidence_oversized");
    // Oversized item selector: same bound.
    let ids: Vec<u64> = (1..=300).collect();
    let resp = client
        .post(format!(
            "{base}/native/evidence/7/retrieve?session={}",
            row.id.raw()
        ))
        .header("x-faktor-server-password", pw.as_str())
        .json(&serde_json::json!({"selector": "items", "ids": ids}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // A bounded selector on owned evidence actually retrieves.
    let resp = client
        .post(format!(
            "{base}/native/evidence/7/retrieve?session={}",
            row.id.raw()
        ))
        .header("x-faktor-server-password", pw.as_str())
        .json(&serde_json::json!({"selector": "all"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["bytesBase64"], "aGVsbG8=");
    assert_eq!(body["byteLen"], 5);
    let _ = handle.request_shutdown();
}

// ---------------------------------------------- native semantic (audit 83)

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_budget_ack_failure_is_typed_retriable_and_never_claimed_applied() {
    // ACK == durable at the wire: an authoritative control-ack failure
    // must surface as a typed RETRIABLE failure (503 persistence_failed),
    // never a silent 200 applied:false and never a permanent 500 — and
    // the child mirror must not claim the change.
    let dir = tempfile::tempdir().unwrap();
    let deps = test_deps(dir.path());
    let orch = deps.orchestrator.clone();
    let token = deps.auth_token.clone();
    let manager = deps.session.clone();
    let handle = serve(deps, 0).await.unwrap();
    let base = format!("http://{}", handle.addr);
    let (_parent, owner, isolated) = orch_owner_env(&manager, dir.path());
    let config = faktor_orchestrator::runtime::ExecConfig {
        run_id: "run-ack-wire".into(),
        ceilings: faktor_orchestrator::runtime::Ceilings::default(),
        parent_caps: read_workspace_caps(),
        provider: "fake".into(),
        default_model: "m".into(),
        isolated_root: isolated.clone(),
        crash_seam: Some(faktor_orchestrator::runtime::CrashSeam::BeforeDrive),
    };
    let res = orch
        .execute_task(
            analysis_plan(&["a"]),
            owner,
            config,
            &[read_child_spec("a")],
        )
        .await
        .expect_err("the seam must fire");
    assert!(
        matches!(
            res,
            faktor_orchestrator::runtime::ExecError::InjectedCrashSeam(_)
        ),
        "{res:?}"
    );
    // The control-row ACK (an UPDATE of the orchestrator_ctl fact) fails
    // while its INSERT succeeds.
    manager
        .store()
        .sql_execute(
            "CREATE TRIGGER fail_ctl_ack BEFORE UPDATE ON memory_fact \
                 WHEN NEW.kind = 'orchestrator_ctl' \
                 BEGIN SELECT RAISE(ABORT, 'injected ack failure'); END;",
        )
        .unwrap();
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/native/agents/child-0/budget"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"max_tokens": 5_000}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "a failed ack is a retriable failure");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "persistence_failed");
    assert_eq!(body["error"]["retryable"], true);
    // Nothing claimed applied: the mirror keeps the pre-change budget and
    // the durable control row is still pending.
    let child = orch.child("child-0").unwrap().unwrap();
    assert_ne!(child.budget_max_tokens, Some(5_000));
    let child_session = manager
        .get_session(SessionId::new(child.session_id))
        .unwrap()
        .unwrap();
    let pending = child_session.orchestrator_ctl_pending().unwrap();
    assert!(
        pending.iter().any(|r| matches!(
            r.control,
            faktor_session::child::ChildControl::ChangeBudget { max_tokens: 5_000 }
        )),
        "the failed change left its durable row unapplied"
    );
    // Remove the seam: the retried call is idempotent and reports applied.
    manager
        .store()
        .sql_execute("DROP TRIGGER fail_ctl_ack;")
        .unwrap();
    let resp = client
        .post(format!("{base}/native/agents/child-0/budget"))
        .bearer_auth(token.as_str())
        .json(&serde_json::json!({"max_tokens": 5_000}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ack: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(ack["applied"], true);
    let _ = handle.request_shutdown();
}
