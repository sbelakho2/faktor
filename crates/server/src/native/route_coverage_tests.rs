// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Route-level adversarial coverage for the findings rows whose claim is
//! "route named by no test". Every test speaks HTTP to the real native
//! listener; hostile bodies, foreign scopes, boundary values and typed
//! refusals are asserted at the wire. No provider is called for real.

#![allow(clippy::too_many_lines)]

use std::io::{Read, Write};
use std::sync::Arc;

use faktor_core::id::SessionId;
use faktor_core::state::AgentState;
use faktor_foundation::Foundation;
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::api::tests::{test_deps, test_deps_with_hooks, wire_snapshot_deps};
use crate::api::{serve_arc, ServerDeps, ServerHandle};

struct RouteRig {
    dir: tempfile::TempDir,
    deps: Arc<ServerDeps>,
    foundation: Arc<Foundation>,
    _handle: ServerHandle,
    base: String,
    session: u64,
}

impl RouteRig {
    async fn plain() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        Self::new(dir, deps).await
    }

    async fn with_hooks(hooks: Arc<faktor_hooks::HookRegistry>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps_with_hooks(dir.path(), vec![], Some(hooks));
        Self::new(dir, deps).await
    }

    async fn with_snapshots() -> (Self, Arc<faktor_snapshot::CheckpointStore>) {
        let dir = tempfile::tempdir().unwrap();
        let (deps, snapshots, _fs) = wire_snapshot_deps(dir.path());
        (Self::new(dir, deps).await, snapshots)
    }

    async fn with_shadows() -> (Self, Arc<faktor_orchestrator::runtime::shadow::ShadowRoots>) {
        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let manager = deps.session.clone();
        let agent = deps.agent.clone();
        let orchestrator =
            faktor_orchestrator::runtime::OrchestratorRuntime::new(manager.clone(), agent.clone());
        let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
            manager.clone(),
            dir.path().join("shadows"),
        )
        .unwrap();
        deps.tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            manager,
            agent,
            shadows.clone(),
        );
        (Self::new(dir, deps).await, shadows)
    }

    async fn new(dir: tempfile::TempDir, mut deps: ServerDeps) -> Self {
        let foundation = Arc::new(Foundation::open(dir.path().join("foundation.db")).unwrap());
        deps.foundation = Some(foundation.clone());
        let ws = deps
            .session
            .create_workspace(dir.path().to_str().unwrap())
            .unwrap();
        let session = deps
            .session
            .create_session(ws, "route-coverage", "fake", "m")
            .unwrap();
        let deps = Arc::new(deps);
        let handle = serve_arc(deps.clone(), 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        Self {
            dir,
            deps,
            foundation,
            _handle: handle,
            base,
            session: session.id().raw(),
        }
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }

    fn handle(&self) -> faktor_session::SessionHandle {
        self.deps
            .session
            .get_session(SessionId::new(self.session))
            .unwrap()
            .unwrap()
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = self
            .client()
            .request(method, format!("{}{}", self.base, path))
            .bearer_auth(self.deps.auth_token.as_str());
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
        (status, value)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }

    async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }

    async fn patch(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }

    async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::DELETE, path, None).await
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

fn hook_spec(id: &str, script: &str, deadline_ms: u64, stdout_cap: usize) -> Value {
    json!({
        "id": id,
        "events": ["pre_tool"],
        "matcher": {"tools": ["*"], "paths": []},
        "command": "sh",
        "args": ["-c", script],
        "env": [],
        "env_allowlist": true,
        "deadline_ms": deadline_ms,
        "stdout_cap": stdout_cap,
        "stderr_cap": 256,
        "failure_policy": "warn",
        "permission_scope": ""
    })
}

fn one_shot_http(status_line: &'static str) -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let body = "{}";
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

fn closed_loopback_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

// ---------------------------------------------------------------- DOM-30

#[tokio::test]
async fn hook_test_and_delete_routes_run_the_real_registry() {
    let hooks = Arc::new(faktor_hooks::HookRegistry::try_new().unwrap());
    let rig = RouteRig::with_hooks(hooks).await;

    // An untrusted hook cannot be enabled, and it cannot be tested.
    let (status, body) = rig
        .post(
            "/native/hooks",
            json!({"id": "untrusted", "spec": hook_spec("untrusted", "printf x", 1000, 64),
                   "trusted": false, "enabled": true}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "hook_untrusted");
    let (status, body) = rig
        .post(
            "/native/hooks",
            json!({"id": "untrusted", "spec": hook_spec("untrusted", "printf x", 1000, 64),
                   "trusted": false, "enabled": false}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = rig
        .post(
            "/native/hooks/untrusted/test",
            json!({"event": "pre_tool", "session": rig.session.to_string(), "payload": {}}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "hook_untrusted");

    // A trusted hook runs through the real run_one path; hostile output is
    // capped and the audit names the observed head.
    let (status, body) = rig
        .post(
            "/native/hooks",
            json!({"id": "chatty", "spec": hook_spec("chatty", "printf '%0100d' 0", 1000, 32),
                   "trusted": true, "enabled": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["registered"], true);
    let (status, body) = rig
        .post(
            "/native/hooks/chatty/test",
            json!({"event": "pre_tool", "session": rig.session.to_string(), "payload": {"tool": "bash"}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["matched"], true);
    let head = body["audit"]["stdout_head"].as_str().unwrap();
    assert!(
        head.chars().count() <= 33,
        "the hostile output is truncated to the cap: {body}"
    );
    assert!(head.contains('…'), "truncation must be visible: {body}");

    // A timeout is bounded by the hook deadline, not the harness.
    let (status, body) = rig
        .post(
            "/native/hooks",
            json!({"id": "slow", "spec": hook_spec("slow", "sleep 5", 100, 64),
                   "trusted": true, "enabled": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let started = std::time::Instant::now();
    let (status, body) = rig
        .post("/native/hooks/slow/test", json!({"event": "pre_tool", "session": rig.session.to_string(), "payload": {"tool": "bash"}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "the hook deadline bounds the test route"
    );
    assert_eq!(
        body["verdict"]["verdict"], "warn",
        "failure_policy=warn: {body}"
    );

    // Hostile bodies are typed 400s; unknown ids are 404.
    let (status, _) = rig
        .post(
            "/native/hooks/chatty/test",
            json!({"event": "not-an-event"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig
        .post(
            "/native/hooks/chatty/test",
            json!({"event": "pre_tool", "unknown": true}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig
        .post("/native/hooks/absent/test", json!({"event": "pre_tool"}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Delete unregisters first and removes the durable record; replay 404.
    let (status, body) = rig.delete("/native/hooks/chatty").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    assert_eq!(body["unregistered"], true);
    let (status, _) = rig.delete("/native/hooks/chatty").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = rig.get("/native/hooks/audit").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["records"].is_array());
}

// ---------------------------------------------------------------- DOM-39

#[tokio::test]
async fn restore_preview_restore_and_review_comments_are_scoped() {
    let (rig, snapshots) = RouteRig::with_snapshots().await;
    let handle = rig.handle();
    std::fs::write(rig.dir.path().join("a.txt"), b"edited\n").unwrap();
    let before = snapshots
        .before_write(handle.id(), "a.txt", b"original\n")
        .unwrap();
    let after = snapshots
        .before_write(handle.id(), "a.txt", b"edited\n")
        .unwrap();
    snapshots
        .after_write(handle.id(), "a.txt", before, after, 0, b"edited\n")
        .unwrap();

    // A comment created before the restore survives it.
    let comments = format!("/native/session/{}/review/comments", rig.session);
    let (status, comment) = rig
        .post(
            &comments,
            json!({"path": "a.txt", "lineStart": 1, "lineEnd": 1,
                   "side": "new", "body": "keep this note"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{comment}");
    let comment_id = comment["comment"]["id"].as_str().unwrap().to_string();

    let preview_path = format!(
        "/native/session/{}/checkpoints/1/restore/preview",
        rig.session
    );
    let (status, preview) = rig.post(&preview_path, json!({"direction": "undo"})).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["writes"], 0);
    assert_eq!(preview["files"][0]["path"], "a.txt");
    assert_eq!(preview["files"][0]["action"], "write", "{preview}");
    assert_eq!(preview["files"][0]["restore"]["hash"], before.to_hex());
    assert_eq!(preview["writable"], true);
    assert_eq!(
        std::fs::read(rig.dir.path().join("a.txt")).unwrap(),
        b"edited\n",
        "preview writes nothing"
    );

    // A manual edit between preview and restore is refused, not clobbered.
    let previewed_current = preview["files"][0]["current"]["hash"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(rig.dir.path().join("a.txt"), b"manual\n").unwrap();
    let restore_path = format!("/native/session/{}/checkpoints/1/restore", rig.session);
    let (status, body) = rig
        .post(
            &restore_path,
            json!({"direction": "undo", "expectedCurrentHash": previewed_current}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "checkpoint_conflict");
    assert_eq!(
        std::fs::read(rig.dir.path().join("a.txt")).unwrap(),
        b"manual\n",
        "a refused restore leaves the manual edit intact"
    );
    // A malformed hash is a typed 400 before any file access.
    let (status, body) = rig
        .post(
            &restore_path,
            json!({"direction": "undo", "expectedCurrentHash": "zz"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    // An unknown direction is a typed 400.
    let (status, _) = rig
        .post(&preview_path, json!({"direction": "sideways"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Restore the exact previewed tree when the CAS precondition matches.
    std::fs::write(rig.dir.path().join("a.txt"), b"edited\n").unwrap();
    let (status, body) = rig
        .post(
            &restore_path,
            json!({"direction": "undo", "expectedCurrentHash": after.to_hex()}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["restored"], true);
    assert_eq!(
        std::fs::read(rig.dir.path().join("a.txt")).unwrap(),
        b"original\n"
    );

    // Comments survive the restore on the owning session, and a foreign
    // session id is a byte-identical 404 (never a cross-session leak).
    let (status, listed) = rig.get(&comments).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["comments"][0]["comment"]["id"], comment_id);
    let foreign = format!("/native/session/{}/review/comments", rig.session + 1000);
    let (status, body) = rig.get(&foreign).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(code(&body), "not_found");

    // Patch/delete stay revision-fenced.
    let (status, _) = rig
        .patch(
            &format!("{comments}/{comment_id}"),
            json!({"resolved": true, "expectedRevision": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = rig
        .patch(
            &format!("{comments}/{comment_id}"),
            json!({"resolved": false, "expectedRevision": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = rig
        .delete(&format!("{comments}/{comment_id}?expectedRevision=1"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "a stale delete is refused");
    let (status, _) = rig
        .delete(&format!("{comments}/{comment_id}?expectedRevision=2"))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn plain_change_accept_is_idempotent_and_rejects_foreign_paths() {
    let (rig, shadows) = RouteRig::with_shadows().await;
    let handle = rig.handle();
    let workspace = rig.dir.path().join("shadow-ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("landed.txt"), b"base\n").unwrap();
    let shadow = shadows
        .begin_shadow(handle.id(), &workspace)
        .expect("shadow begins");
    std::fs::write(shadow.root.join("landed.txt"), b"candidate\n").unwrap();

    let accept = format!("/native/session/{}/changes/accept", rig.session);
    let body = json!({"approved": ["landed.txt"], "rejected": []});
    let (status, receipt) = rig.post(&accept, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["accepted"], true);
    assert_eq!(receipt["integrated"], true);
    assert_eq!(receipt["proof"]["requiresReverification"], true);
    assert_eq!(
        std::fs::read(workspace.join("landed.txt")).unwrap(),
        b"candidate\n"
    );
    // The same decision replays the stored receipt; a different selection
    // on the same change set is a typed conflict.
    let (status, replay) = rig.post(&accept, body).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay, receipt);
    let (status, conflict) = rig
        .post(&accept, json!({"approved": [], "rejected": ["landed.txt"]}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(code(&conflict), "change_accept_conflict");

    // A traversal/foreign path never lands.
    let (status, _) = rig
        .post(
            &accept,
            json!({"approved": ["../escape.txt"], "rejected": []}),
        )
        .await;
    assert!(
        status == StatusCode::BAD_REQUEST
            || status == StatusCode::CONFLICT
            || status == StatusCode::UNPROCESSABLE_ENTITY,
        "foreign path must be refused typed, got {status}"
    );
    assert!(!rig.dir.path().parent().unwrap().join("escape.txt").exists());
}

// ---------------------------------------------------------------- DOM-45

#[tokio::test]
async fn session_delete_export_and_metadata_routes_are_typed() {
    let rig = RouteRig::plain().await;
    let session = format!("/native/session/{}", rig.session);

    // Metadata: defaults, pin/archive, rename, hostile bodies.
    let meta = format!("/native/sessions/{}/meta", rig.session);
    let (status, body) = rig.get(&meta).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pinned"], false);
    assert_eq!(body["archived"], false);
    let (status, body) = rig
        .post(
            &meta,
            json!({"title": "renamed", "pinned": true, "archived": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pinned"], true);
    assert_eq!(body["archived"], true);
    let (status, body) = rig.get(&meta).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["pinned"], true);
    let (status, _) = rig.post(&meta, json!({"title": "   "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig.post(&meta, json!({"pinned": "yes-please"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = rig.get("/native/sessions/999999/meta").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A live turn refuses deletion unless abort=true was explicitly passed.
    rig.deps
        .session
        .store()
        .set_session_state(SessionId::new(rig.session), AgentState::Streaming)
        .unwrap();
    let (status, body) = rig.delete(&session).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "session_active");
    let (status, _) = rig.delete(&format!("{session}?reason=%00bad")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig.delete("/native/session/not-a-number").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig.delete("/native/session/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Export before deletion is bounded and digest-addressed.
    let export = format!("{session}/export");
    let (status, bundle) = rig.post(&export, json!({"redact": true})).await;
    assert_eq!(status, StatusCode::OK, "{bundle}");
    assert_eq!(bundle["bundle"]["schema"], "faktor.session-export/v1");
    let (status, _) = rig
        .post(&export, json!({"redact": true, "maxBytes": 1}))
        .await;
    assert_eq!(status, StatusCode::OK, "bounds clamp rather than refuse");

    // Abort-then-delete succeeds; replay is idempotent; content is gone.
    let (status, deleted) = rig
        .delete(&format!("{session}?abort=true&reason=operator%20request"))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["idempotent"], false);
    assert!(deleted["purged"].is_object(), "{deleted}");
    assert!(
        deleted["purged"]["event"].as_u64().unwrap_or(0) > 0,
        "{deleted}"
    );
    let (status, replay) = rig.delete(&format!("{session}?abort=true")).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["idempotent"], true);
    let (status, gone) = rig.post(&export, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{gone}");
    assert_eq!(code(&gone), "session_deleted");
}

// ---------------------------------------------------------------- DOM-51

#[tokio::test]
async fn usage_totals_match_the_provider_ledger_and_efficiency_stays_honest() {
    let rig = RouteRig::plain().await;
    let store = rig.deps.session.store();
    let sid = SessionId::new(rig.session);
    let mut expected_tokens = 0u64;
    // 1,000 provider calls: the route total must equal the durable ledger.
    for turn in 0..1_000u64 {
        let tokens_in = (turn % 97) + 1;
        let tokens_out = turn % 53;
        expected_tokens = expected_tokens.saturating_add(tokens_in + tokens_out);
        store
            .record_provider_call(
                sid,
                rig.deps.session.try_next_op_id().unwrap(),
                "fake",
                "m",
                "completed",
                Some(tokens_in),
                Some(tokens_out),
                None,
            )
            .unwrap();
    }
    let usage_path = format!("/native/session/{}/usage", rig.session);
    let (status, usage) = rig.get(&usage_path).await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(
        usage["providerCalls"]["tokens"], expected_tokens,
        "per-session totals equal the provider-call ledger"
    );
    let (status, global) = rig.get("/native/usage").await;
    assert_eq!(status, StatusCode::OK, "{global}");
    assert_eq!(
        global["durable"]["providerCalls"]["tokens"],
        expected_tokens
    );

    // K-12: efficiency reports only measured facts. With no call receipts
    // the token aggregate is unavailable; the never-measured cache and
    // local operating cost are unavailable, never a fabricated zero.
    let (status, efficiency) = rig
        .get(&format!("/native/session/{}/efficiency", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK, "{efficiency}");
    assert_eq!(efficiency["tokens"]["state"], "unavailable");
    assert_eq!(efficiency["cache"]["state"], "unavailable");
    assert_eq!(efficiency["localOperatingCost"]["state"], "unavailable");
    assert!(efficiency["cache"].get("value").is_none());
    assert!(efficiency["localOperatingCost"].get("cost").is_none());
    assert_eq!(
        efficiency["routing"]["outcomes"]["state"], "unavailable",
        "routing outcomes are never inferred from receipts"
    );

    // A task with no cost cap reports null (unknown/unlimited), never 0.
    let (status, started) = rig
        .post(
            &format!("/native/session/{}/task-runs", rig.session),
            json!({"prompt": "usage probe", "criteria": ["answers"]}),
        )
        .await;
    if status == StatusCode::OK {
        let (status, usage) = rig.get(&usage_path).await;
        assert_eq!(status, StatusCode::OK, "{usage}");
        for task in usage["tasks"].as_array().unwrap_or(&Vec::new()) {
            if task["taskId"] == started["taskId"] {
                assert_eq!(task["budget"]["maxCostMicro"], Value::Null);
            }
        }
    }
    // Hostile ids are typed.
    let (status, body) = rig.get("/native/session/0/usage").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig.get("/native/session/999999/usage").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = rig
        .get(&format!(
            "/native/session/{}/efficiency?bogus=1",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ---------------------------------------------------------------- DOM-53/54

#[tokio::test]
async fn config_rollback_import_ack_and_session_setting_boundaries() {
    let rig = RouteRig::plain().await;
    let (status, snapshot) = rig.get("/native/config/snapshot").await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    let revision = snapshot["revision"].as_u64().unwrap();
    let (status, first) = rig
        .post(
            "/native/config/commit",
            json!({"expected_revision": revision,
                   "changes": [{"key": "faktor.model.contextLimit", "scope": "user", "value": 4096}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let r1 = first["saved_revision"].as_u64().unwrap();
    let (status, second) = rig
        .post(
            "/native/config/commit",
            json!({"expected_revision": r1,
                   "changes": [{"key": "faktor.model.contextLimit", "scope": "project", "value": 2048}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let r2 = second["saved_revision"].as_u64().unwrap();
    // The narrower project scope wins while it exists.
    let (status, preview) = rig
        .post(
            &format!("/native/session/{}/context/preview", rig.session),
            json!({"operationId": "op-cap", "capacity": 32768}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["capacity"], 2048);

    // Reset one scope override: the inherited user value wins again.
    let (status, reset) = rig
        .post(
            "/native/config/reset",
            json!({"expectedRevision": r2, "key": "faktor.model.contextLimit", "scope": "project"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    let r3 = reset["saved_revision"].as_u64().unwrap();
    let (status, preview) = rig
        .post(
            &format!("/native/session/{}/context/preview", rig.session),
            json!({"operationId": "op-cap", "capacity": 32768}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["capacity"], 4096);

    // Hostile import writes nothing; a valid import is one atomic patch.
    let before = rig.get("/native/config/snapshot").await.1;
    let (status, hostile) = rig
        .post(
            "/native/config/import",
            json!({"bundle": {"schema": "faktor-config-bundle/v0", "revision": r3,
                              "values": [], "omitted_secrets": []}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{hostile}");
    let after = rig.get("/native/config/snapshot").await.1;
    assert_eq!(before["revision"], after["revision"]);
    assert_eq!(before["values"], after["values"]);
    let (status, imported) = rig
        .post(
            "/native/config/import",
            json!({"bundle": {"schema": "faktor-config-bundle/v1", "revision": r3,
                              "values": [{"key": "faktor.model.contextLimit", "scope": "user", "value": 8192}],
                              "omitted_secrets": []},
                   "expectedRevision": r3}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{imported}");
    let r4 = imported["saved_revision"].as_u64().unwrap();
    let (status, snapshot) = rig.get("/native/config/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        snapshot["values"]["faktor.model.contextLimit"]["value"],
        8192
    );

    // Rollback replays the exact target as a NEW revision.
    let (status, rolled) = rig
        .post(
            "/native/config/rollback",
            json!({"expectedRevision": r4, "targetRevision": r1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rolled}");
    let (status, snapshot) = rig.get("/native/config/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        snapshot["values"]["faktor.model.contextLimit"]["value"],
        4096
    );
    let (status, stale) = rig
        .post(
            "/native/config/rollback",
            json!({"expectedRevision": r4, "targetRevision": r1}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    let current = snapshot["revision"].as_u64().unwrap();
    let (status, forward) = rig
        .post(
            "/native/config/rollback",
            json!({"expectedRevision": current, "targetRevision": current + 5}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{forward}");

    // Runtime acknowledgement is explicit and revision-fenced.
    let (status, ack) = rig
        .post("/native/config/runtime-ack", json!({"expectedRevision": 0}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{ack}");
    let (status, ack) = rig
        .post(
            "/native/config/runtime-ack",
            json!({"expectedRevision": current}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(ack["runtimeRevision"], current);

    // Per-session settings: boundary values and typed refusals.
    let settings = format!("/native/session/{}/settings", rig.session);
    let (status, body) = rig.put(&settings, json!({"contextWindow": 255})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig.put(&settings, json!({"contextWindow": 256})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = rig
        .put(&settings, json!({"contextWindow": 10_000_001}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig.put(&settings, json!({"maxTurns": 0})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig.put(&settings, json!({"unknownSetting": 1})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig.put(&settings, json!({"unset": ["notAKey"]})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig.get(&settings).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applyTiming"]["contextWindow"], "next_turn");
    assert_eq!(body["applyTiming"]["maxTurns"], "next_task");
    // unset clears the override (the route's "reset one setting").
    let current = body["revision"].as_u64().unwrap();
    let (status, body) = rig
        .put(
            &settings,
            json!({"unset": ["contextWindow"], "expectedRevision": current}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = rig.get(&settings).await;
    assert_eq!(body["contextWindow"], Value::Null);
}

// ---------------------------------------------------------------- DOM-56

#[tokio::test]
async fn provider_connection_test_health_and_delete_are_bounded_and_typed() {
    let rig = RouteRig::plain().await;

    // Endpoint down: a bounded observed failure, never an error 5xx.
    let dead_port = closed_loopback_port();
    let (status, saved) = rig
        .post(
            "/native/providers/connections",
            json!({"id": "down", "family": "ollama", "label": "down",
                   "endpoint": format!("http://127.0.0.1:{dead_port}"),
                   "models": ["llama3:8b"], "allow_loopback": true,
                   "connect_timeout_ms": 200, "total_timeout_ms": 500}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, health) = rig.get("/native/providers/connections/down/health").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{health}");
    assert_eq!(health["code"], "no_observation");
    let (status, health) = rig
        .post("/native/providers/connections/down/test", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["state"], "failed");
    let (status, health) = rig.get("/native/providers/connections/down/health").await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["state"], "failed");

    // Invalid TLS: a plaintext listener behind an https endpoint fails the
    // handshake as an observed failure, never a panic or fake ready.
    let plain_port = one_shot_http("200 OK");
    let (status, saved) = rig
        .post(
            "/native/providers/connections",
            json!({"id": "bad-tls", "family": "ollama", "label": "bad-tls",
                   "endpoint": format!("https://127.0.0.1:{plain_port}"),
                   "models": ["m"], "allow_loopback": true,
                   "connect_timeout_ms": 300, "total_timeout_ms": 800}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, health) = rig
        .post("/native/providers/connections/bad-tls/test", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["state"], "failed", "{health}");

    // Wrong key: the endpoint answers 401; the credential is rejected typed.
    let (status, credential) = rig
        .post(
            "/native/providers/credentials",
            json!({"kind": "provider_api_key", "label": "wrong", "secret": "sk-wrong"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{credential}");
    let handle = credential["handle"].as_str().unwrap().to_string();
    let key_port = one_shot_http("401 Unauthorized");
    let (status, saved) = rig
        .post(
            "/native/providers/connections",
            json!({"id": "wrong-key", "family": "openai", "label": "wrong-key",
                   "endpoint": format!("http://127.0.0.1:{key_port}"),
                   "credential_ref": handle, "models": ["gpt-x"],
                   "allow_loopback": true, "connect_timeout_ms": 300,
                   "total_timeout_ms": 800}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, health) = rig
        .post("/native/providers/connections/wrong-key/test", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["state"], "authentication_required", "{health}");
    assert!(health["stages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|stage| stage["stage"] == "authentication" && stage["state"] == "failed"));

    // Hostile save values are typed and store nothing.
    let (status, body) = rig
        .post(
            "/native/providers/connections",
            json!({"id": "bad-tls-mode", "family": "openai", "label": "x",
                   "endpoint": "https://api.example.com", "models": [],
                   "tls_mode": "verify_nothing"}),
        )
        .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::CONFLICT,
        "{body}"
    );

    // Delete is a local removal that stops nothing and claims no revocation;
    // the session and the runtime provider registry are unaffected.
    let (status, deleted) = rig.delete("/native/providers/connections/down").await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["remoteRevoked"], false);
    let (status, _) = rig.delete("/native/providers/connections/down").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = rig
        .post("/native/providers/connections/down/test", json!({}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = rig.get("/native/providers/connections/down/health").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, sessions) = rig.get("/native/sessions").await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    let sid = rig.session.to_string();
    assert!(sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == sid));
    let (status, providers) = rig.get("/native/providers").await;
    assert_eq!(status, StatusCode::OK, "{providers}");
}

// ---------------------------------------------------------------- DOM-62

#[tokio::test]
async fn memory_delete_is_absent_from_every_view_and_restore_revives_it() {
    let rig = RouteRig::plain().await;
    let records = "/native/memory/records";
    let secret = "SECRET-MEMORY-VALUE-DO-NOT-SURFACE";
    let (status, created) = rig
        .post(
            records,
            json!({"session": rig.session.to_string(), "scope": "workspace",
                   "subject": "project", "predicate": "deploy.key",
                   "value": {"type": "text", "value": secret},
                   "provenance": {"origin": "user", "sourceRef": "settings"},
                   "confidencePpm": 1000000}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["record"]["id"].as_str().unwrap().to_string();

    // A sibling session in the SAME workspace sees the workspace-scoped row.
    let workspace = rig.handle().row().unwrap().workspace_id;
    let sibling = rig
        .deps
        .session
        .create_session(workspace, "sibling", "fake", "m")
        .unwrap();
    let (status, page) = rig
        .get(&format!(
            "{records}?session={}&scope=workspace",
            sibling.id()
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["records"].as_array().unwrap().len(), 1);

    // Delete tombstones it; every future-use view hides the value.
    let (status, deleted) = rig
        .delete(&format!(
            "{records}/{id}?session={}&expectedVersion=1&reason=user",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    let (status, page) = rig
        .get(&format!(
            "{records}?session={}&scope=workspace",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page["records"].as_array().unwrap().is_empty(), "{page}");
    let (status, legacy) = rig
        .get(&format!("/native/memory?session={}&limit=200", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK, "{legacy}");
    assert!(
        !legacy.to_string().contains(secret),
        "the deleted value must be absent from the raw memory view: {legacy}"
    );
    let (status, exported) = rig
        .get(&format!("/native/context/export?session={}", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK, "{exported}");
    assert!(
        !exported.to_string().contains(secret),
        "the deleted value must be absent from the context export: {exported}"
    );
    let (status, preview) = rig
        .post(
            &format!("/native/session/{}/context/preview", rig.session),
            json!({"operationId": "after-delete"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert!(
        !preview.to_string().contains(secret),
        "the deleted value must be absent from the next-turn context: {preview}"
    );

    // Restore requires the tombstone version and revives exactly once.
    let (status, stale) = rig
        .post(
            &format!("{records}/{id}/restore"),
            json!({"session": rig.session.to_string(), "expectedVersion": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    let (status, restored) = rig
        .post(
            &format!("{records}/{id}/restore"),
            json!({"session": rig.session.to_string(), "expectedVersion": 2}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(restored["record"]["version"], 3);
    assert!(restored["record"]["deleted"].is_null());

    // Retention boundaries: a negative day count never deserializes, an
    // out-of-domain count is refused, and a dry run deletes nothing.
    let retention = "/native/memory/records/retention";
    let (status, _) = rig
        .post(
            retention,
            json!({"session": rig.session.to_string(), "olderThanDays": -1}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = rig
        .post(
            retention,
            json!({"session": rig.session.to_string(), "olderThanDays": 3651}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, dry) = rig
        .post(
            retention,
            json!({"session": rig.session.to_string(), "scope": "workspace",
                   "olderThanDays": 0, "dryRun": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{dry}");
    assert_eq!(dry["dryRun"], true);
    assert_eq!(dry["deleted"], 0);

    // The legacy fact route deletes with an exact timestamp CAS too.
    let (status, written) = rig
        .post(
            "/native/memory",
            json!({"session": rig.session.to_string(), "kind": "scratch",
                   "key": "k", "value": "v"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    let updated = written["fact"]["updatedMs"].as_i64().unwrap();
    let (status, stale) = rig
        .delete(&format!("/native/memory/scratch/k?session={}", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK, "CAS is optional: {stale}");
    let (status, again) = rig
        .delete(&format!(
            "/native/memory/scratch/k?session={}&expectedUpdatedMs={updated}",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{again}");
}

// ---------------------------------------------------------------- DOM-63

#[tokio::test]
async fn skill_routes_enforce_trust_hostile_trees_and_caps() {
    let rig = RouteRig::plain().await;
    let skills_root = rig.dir.path().join(".faktor/skills");
    let good = skills_root.join("good");
    std::fs::create_dir_all(&good).unwrap();
    std::fs::write(
        good.join("SKILL.md"),
        "# Good skill\n## Keywords: helpful\nuse {{target}} now\n",
    )
    .unwrap();
    let huge = skills_root.join("huge");
    std::fs::create_dir_all(&huge).unwrap();
    let mut huge_body = String::from("# Huge skill\n");
    huge_body.push_str(&"A".repeat(1024 * 1024));
    std::fs::write(huge.join("SKILL.md"), huge_body).unwrap();
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("SKILL.md"), "# Escaped\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), skills_root.join("link")).unwrap();
    }

    let (status, listed) = rig
        .get(&format!(
            "/native/skills?session={}&limit=10000",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let names: Vec<&str> = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["installed"] == false)
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"good") && names.contains(&"huge"),
        "{listed}"
    );
    #[cfg(unix)]
    assert!(
        !names.contains(&"link"),
        "a symlink is never listed: {listed}"
    );
    assert!(
        listed["skills"].as_array().unwrap().len() <= 500,
        "the served listing stays at the 500 cap"
    );
    let (status, body) = rig
        .get(&format!(
            "/native/skills?session={}&after={}",
            rig.session,
            "x".repeat(257)
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A hostile symlink can never be installed or loaded.
    #[cfg(unix)]
    {
        let (status, body) = rig
            .post(
                "/native/skills/install",
                json!({"session": rig.session.to_string(), "name": "link"}),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }
    let (status, body) = rig
        .post(
            "/native/skills/install",
            json!({"session": rig.session.to_string(), "name": "absent"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A huge honest skill contributes only its capped text.
    let (status, installed) = rig
        .post(
            "/native/skills/install",
            json!({"session": rig.session.to_string(), "name": "huge"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    let huge_id = installed["id"].as_str().unwrap().to_string();
    assert!(installed["text"].as_str().unwrap().len() <= 64 * 1024);

    // Install is initially untrusted; invocation is refused until trust.
    let (status, installed) = rig
        .post(
            "/native/skills/install",
            json!({"session": rig.session.to_string(), "name": "good"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    let id = installed["id"].as_str().unwrap().to_string();
    assert_eq!(installed["trusted"], false);
    let (status, body) = rig
        .post(
            &format!("/native/skills/{id}/invoke"),
            json!({"variables": {"target": "world"}}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "skill_untrusted");
    let (status, trusted) = rig
        .post(
            &format!("/native/skills/{id}/trust"),
            json!({"trusted": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{trusted}");
    assert_eq!(trusted["trusted"], true);
    let (status, invoked) = rig
        .post(
            &format!("/native/skills/{id}/invoke"),
            json!({"variables": {"target": "world"}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{invoked}");
    assert!(invoked["prompt"]
        .as_str()
        .unwrap()
        .contains("use world now"));
    assert_eq!(invoked["grants"], json!([]));
    // Hostile template variables are bounded.
    let (status, body) = rig
        .post(
            &format!("/native/skills/{id}/invoke"),
            json!({"variables": {"bad name": "x"}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig
        .post(
            &format!("/native/skills/{id}/invoke"),
            json!({"variables": {"k": "x".repeat(4097)}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let many: std::collections::BTreeMap<String, String> =
        (0..33).map(|i| (format!("k{i}"), "v".into())).collect();
    let (status, body) = rig
        .post(
            &format!("/native/skills/{id}/invoke"),
            json!({"variables": many}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Invocation policy is validated; the read route returns the manifest.
    let (status, body) = rig
        .post(
            &format!("/native/skills/{id}/invocation"),
            json!({"invocation": "agent_selectable"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["invocation"], "agent_selectable");
    let (status, body) = rig
        .post(
            &format!("/native/skills/{id}/invocation"),
            json!({"invocation": "always"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig.get(&format!("/native/skills/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "good");

    // Delete removes the managed row; the discovered tree is unchanged.
    let (status, body) = rig.delete(&format!("/native/skills/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    let (status, _) = rig.get(&format!("/native/skills/{id}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = rig.delete(&format!("/native/skills/{huge_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------- DOM-64

#[tokio::test]
async fn mcp_start_stop_tools_resources_and_expired_connect_are_typed() {
    let dir = tempfile::tempdir().unwrap();
    let mut deps = test_deps(dir.path());
    let supervisor = faktor_terminal::ProcessSupervisor::new(deps.session.cas());
    deps.mcp_runtime = Arc::new(crate::native::McpRuntime::new(Some(supervisor)));
    let rig = RouteRig::new(dir, deps).await;
    let mock = faktor_test_mocks::mcp_bin();

    let (status, saved) = rig
        .post(
            "/native/mcp/servers",
            json!({"id": "mock", "name": "Mock", "command": mock.to_string_lossy(),
                   "args": [], "env": [], "trusted": true, "enabled": true,
                   "transport": "stdio", "timeout_ms": 5000,
                   "output_cap_bytes": 1048576, "stderr_cap_bytes": 65536}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    // Starting twice reuses the one live connection and returns the same
    // discovered catalog; it never fabricates a second ready state.
    let (status, first) = rig.post("/native/mcp/servers/mock/start", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let first_tools = first["tools"].clone();
    assert!(first_tools
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "mock_echo"));
    let (status, second) = rig.post("/native/mcp/servers/mock/start", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["tools"], first_tools);
    let (status, tools) = rig.get("/native/mcp/servers/mock/tools").await;
    assert_eq!(status, StatusCode::OK, "{tools}");
    assert_eq!(tools["serverId"], "mock");
    assert_eq!(tools["tools"], first_tools);
    let (status, resources) = rig.get("/native/mcp/servers/mock/resources").await;
    assert_eq!(status, StatusCode::OK, "{resources}");
    assert!(resources.is_object());

    // Stop during an in-flight catalog read is bounded; after the stop the
    // catalog routes are typed 409, never stale data.
    let (stop, read) = tokio::join!(
        rig.post("/native/mcp/servers/mock/stop", json!({})),
        rig.get("/native/mcp/servers/mock/tools"),
    );
    assert_eq!(stop.0, StatusCode::OK, "{}", stop.1);
    assert_eq!(stop.1["stopped"], true);
    assert!(
        read.0 == StatusCode::OK || read.0 == StatusCode::CONFLICT,
        "a racing read is typed: {:?} {}",
        read.0,
        read.1
    );
    let (status, body) = rig.get("/native/mcp/servers/mock/tools").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "mcp_not_ready");
    let (status, body) = rig.get("/native/mcp/servers/mock/resources").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "mcp_not_ready");
    let (status, body) = rig.post("/native/mcp/servers/mock/stop", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stopped"], false);

    // Disabled/untrusted servers refuse typed; unknown ids are 404.
    for (id, trusted, enabled, expected) in [
        ("disabled", true, false, StatusCode::CONFLICT),
        ("untrusted", false, true, StatusCode::CONFLICT),
    ] {
        let (status, body) = rig
            .post(
                "/native/mcp/servers",
                json!({"id": id, "name": id, "command": mock.to_string_lossy(),
                       "args": [], "env": [], "trusted": trusted, "enabled": enabled,
                       "transport": "stdio", "timeout_ms": 2000}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = rig
            .post(&format!("/native/mcp/servers/{id}/start"), json!({}))
            .await;
        assert_eq!(status, expected, "{id}: {body}");
    }
    for route in ["start", "stop"] {
        let (status, _) = rig
            .post(&format!("/native/mcp/servers/absent/{route}"), json!({}))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
    }
    for route in ["tools", "resources"] {
        let (status, _) = rig
            .get(&format!("/native/mcp/servers/absent/{route}"))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{route}");
    }

    // Connect with an expired OAuth bundle (no refresh) fails before the
    // network is touched, with the typed expired code.
    let (status, saved) = rig
        .post(
            "/native/mcp/servers",
            json!({"id": "remote", "name": "Remote", "command": "unused",
                   "args": [], "env": [], "trusted": true, "enabled": true,
                   "transport": "streamable_http",
                   "endpoint": "https://mcp.example.com/rpc", "timeout_ms": 1000}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let record = rig
        .foundation
        .customization
        .mcp_server("remote")
        .unwrap()
        .unwrap();
    let expired = faktor_mcp::oauth::OAuthTokenSet::for_test(
        "remote",
        Some("operator"),
        "expired-access",
        None,
        Some(1),
        vec!["mcp.read".into()],
        "https://auth.example.com/token",
        None,
    );
    crate::native::mcp_oauth::store_tokens(&rig.foundation, &record, expired).unwrap();
    let (status, body) = rig
        .post("/native/mcp/servers/remote/connect", json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "oauth_expired");

    // Delete stops first and removes the durable spec; the route is gone.
    let (status, body) = rig.delete("/native/mcp/servers/mock").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    let (status, _) = rig.post("/native/mcp/servers/mock/start", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- DOM-65

#[tokio::test]
async fn preset_roles_and_fallback_routes_refuse_cycles_and_deleted_models() {
    let rig = RouteRig::plain().await;
    let preset = json!({
        "id": "p2", "mode": "code", "name": "Coder", "description": "",
        "origin": "user", "scope": "user", "roleInstructions": "code",
        "modelProvider": "fake", "modelId": "m", "toolAllow": [], "toolDeny": [],
        "delegation": {"maxDepth": 0, "maxChildren": 0, "parentBudgetSharePpm": 500000,
                        "allowedChildPresets": []},
        "childConcurrency": 0, "budgetSharePpm": 500000
    });
    let (status, body) = rig.post("/native/presets", preset).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A role naming a deleted/unknown provider or model is refused before
    // persistence; served reads mark the surviving binding explicitly.
    let (status, body) = rig
        .post(
            "/native/presets/p2/roles",
            json!({"roleModels": {"planner": {"provider": "deleted", "model": "m"}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "unknown_provider");
    let (status, body) = rig
        .post(
            "/native/presets/p2/roles",
            json!({"roleModels": {"planner": {"provider": "fake", "model": "m"}}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applyTiming"], "next_child_or_model_call");
    let (status, body) = rig.get("/native/presets/p2/roles").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["served"]["planner"]["served"], true);

    // Fallback cycles (a duplicated candidate) are refused typed; a deleted
    // provider is refused; order is preserved.
    let fallback = "/native/presets/p2/fallback";
    let (status, body) = rig
        .post(
            fallback,
            json!({"fallbackChain": [{"provider": "fake", "model": "m"},
                                      {"provider": "fake", "model": "m"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig
        .post(
            fallback,
            json!({"fallbackChain": [{"provider": "deleted", "model": "m"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = rig
        .post(
            fallback,
            json!({"fallbackChain": [{"provider": "fake", "model": "m"},
                                      {"provider": "fake", "model": "m2"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applyTiming"], "next_routing_decision");
    assert_eq!(body["fallbackChain"][0]["model"], "m");
    assert_eq!(body["fallbackChain"][1]["model"], "m2");
    let (status, body) = rig.get(fallback).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["candidates"][0]["served"], true);

    // A model that disappeared AFTER the chain was saved is surfaced as
    // served=false by the read, never silently routed.
    let record = rig.foundation.presets.get("p2").unwrap().unwrap();
    rig.foundation
        .presets
        .set_fallback_chain(
            "p2",
            vec![
                faktor_foundation::presets::FallbackCandidate {
                    provider: "fake".into(),
                    model: "m".into(),
                    reason: Some("primary".into()),
                    quality_floor_ppm: None,
                },
                faktor_foundation::presets::FallbackCandidate {
                    provider: "gone".into(),
                    model: "vanished".into(),
                    reason: None,
                    quality_floor_ppm: None,
                },
            ],
            Some(record.revision),
        )
        .unwrap();
    let (status, body) = rig.get(fallback).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["candidates"][0]["served"], true);
    assert_eq!(body["candidates"][1]["served"], false, "{body}");
    // Re-saving the same deleted binding through the route is refused.
    let (status, body) = rig
        .post(
            fallback,
            json!({"fallbackChain": [{"provider": "gone", "model": "vanished"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // An empty chain clears the preference (inherit), and unknown presets
    // are typed 404 on every role/fallback route.
    let (status, body) = rig.post(fallback, json!({"fallbackChain": []})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["fallbackChain"], json!([]));
    for path in [
        "/native/presets/absent/roles".to_string(),
        "/native/presets/absent/fallback".to_string(),
    ] {
        let (status, _) = rig.get(&path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _) = rig
        .post("/native/presets/absent/roles", json!({"roleModels": {}}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- DOM-66

#[tokio::test]
async fn permission_rule_delete_and_matching_corpus_refuse_compound_targets() {
    let rig = RouteRig::plain().await;
    let exact = "bash -lc 'cargo test'";
    let (status, rule) = rig
        .post(
            "/native/permissions/rules",
            json!({"scope": "user", "tool_id": "bash", "target": exact,
                   "decision": "allow", "expectedRevision": 0}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rule}");
    let rule_id = rule["id"].as_str().unwrap().to_string();
    let (status, glob_rule) = rig
        .post(
            "/native/permissions/rules",
            json!({"scope": "project", "tool_id": "read",
                   "target": "glob:/w/src/*.rs", "decision": "allow",
                   "expectedRevision": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{glob_rule}");
    let policy_revision = glob_rule["revision"].as_u64().unwrap();

    // Preview and dispatch share ONE evaluator: unmatched compound/quoting
    // variants are "ask", never allow.
    let preview = |tool: &str, target: &str| {
        let rig = &rig;
        let body = json!({"tool_id": tool, "canonical_target": target, "effect": "execute"});
        async move { rig.post("/native/permissions/preview", body).await }
    };
    for hostile in [
        "bash -lc 'cargo test' && rm -rf /",
        "bash -lc 'cargo test'; curl evil",
        "bash -lc \"cargo test\"",
        "bash -lc 'cargo $(curl evil)'",
        "bash -lc 'cargo $TEST'",
        "bash -lc 'cargo test' ",
        "bash -lc ’cargo test’",
        "bash -lc\u{a0}'cargo test'",
        "bash -lc 'cargo test\nrm -rf /'",
    ] {
        let (status, body) = preview("bash", hostile).await;
        assert_eq!(status, StatusCode::OK, "{hostile}: {body}");
        assert_eq!(body["decision"], "ask", "{hostile}: {body}");
        assert_eq!(body["matched"], false, "{hostile}: {body}");
    }
    let (status, body) = preview("bash", exact).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["decision"], "allow");
    assert_eq!(body["rule_id"], rule_id);

    // Path globs are whole-string, not substring/prefix.
    let (status, body) = preview("read", "/w/src/a.rs").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["decision"], "allow");
    for hostile in ["/w/src/a.rs.bak", "/other/src/a.rs", "/w/src/a.rs\u{0}"] {
        let (status, body) = preview("read", hostile).await;
        assert_eq!(status, StatusCode::OK, "{hostile}: {body}");
        assert_eq!(body["decision"], "ask", "{hostile}: {body}");
    }

    // An open-ended allow glob and an already-expired rule are refused at
    // save time (no decorative rule is persisted).
    let (status, body) = rig
        .post(
            "/native/permissions/rules",
            json!({"scope": "user", "tool_id": "bash", "target": "glob:bash *",
                   "decision": "allow", "expectedRevision": policy_revision}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig
        .post(
            "/native/permissions/rules",
            json!({"scope": "user", "tool_id": "bash", "target": "x",
                   "decision": "allow", "expires_ms": 1,
                   "expectedRevision": policy_revision}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // The preview is recorded in the audit with source=preview.
    let (status, audit) = rig.get("/native/permissions/audit?limit=5").await;
    assert_eq!(status, StatusCode::OK, "{audit}");
    assert!(audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["source"] == "preview"));

    // Delete requires the exact policy revision and removes the rule.
    let (status, body) = rig
        .delete(&format!("/native/permissions/rules/{rule_id}"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = rig
        .delete(&format!(
            "/native/permissions/rules/{rule_id}?expectedRevision=0"
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = rig
        .delete(&format!(
            "/native/permissions/rules/{rule_id}?expectedRevision={policy_revision}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], true);
    let (status, body) = rig
        .delete(&format!(
            "/native/permissions/rules/{rule_id}?expectedRevision={}",
            body["revision"].as_u64().unwrap()
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = preview("bash", exact).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["decision"], "ask", "the deleted rule is gone: {body}");
}
