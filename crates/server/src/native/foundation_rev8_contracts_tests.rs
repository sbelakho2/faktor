//! Production-wiring tests for the Rev8 field-level daemon contracts. Every
//! test speaks HTTP to the real native listener; nothing uses a private
//! handler seam. Adversarial cases cover hostile bodies, foreign scopes,
//! idempotent replay and typed refusals instead of fake ready states.

use std::sync::Arc;

use faktor_foundation::Foundation;
use reqwest::StatusCode;

use crate::serve_arc;
use serde_json::{json, Value};

use super::foundation_r8::foundation_r8_tests::Rig;

#[tokio::test]
async fn preset_roles_and_fallback_round_trip_with_served_validation() {
    let rig = Rig::new().await;
    let (status, preset) = rig
        .post(
            "/native/presets",
            json!({
                "id": "p1", "mode": "code", "name": "Coder",
                "description": "", "origin": "user", "scope": "user",
                "roleInstructions": "code", "modelProvider": "fake", "modelId": "m",
                "toolAllow": [], "toolDeny": [],
                "delegation": {"maxDepth": 0, "maxChildren": 0,
                    "parentBudgetSharePpm": 500000, "allowedChildPresets": []},
                "childConcurrency": 0, "budgetSharePpm": 500000
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preset}");

    let (status, roles) = rig
        .post(
            "/native/presets/p1/roles",
            json!({"roleModels": {"coder": {"provider": "fake", "model": "m"}}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{roles}");
    assert_eq!(roles["roleModels"]["coder"]["model"], "m");
    let (status, read) = rig.get("/native/presets/p1/roles").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["served"]["coder"]["served"], true);

    let (status, fallback) = rig
        .post(
            "/native/presets/p1/fallback",
            json!({"fallbackChain": [
                {"provider": "fake", "model": "m", "reason": "first"},
                {"provider": "fake", "model": "m", "reason": "dup"}
            ]}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{fallback}");
    let (status, fallback) = rig
        .post(
            "/native/presets/p1/fallback",
            json!({"fallbackChain": [{"provider": "fake", "model": "m"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{fallback}");
    let (status, read) = rig.get("/native/presets/p1/fallback").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["candidates"][0]["served"], true);

    // An unserved provider/model is refused before persistence.
    let (status, refused) = rig
        .post(
            "/native/presets/p1/roles",
            json!({"roleModels": {"planner": {"provider": "nope", "model": "m"}}}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
}

#[tokio::test]
async fn catalog_probe_is_bounded_and_reports_typed_failure_not_ready() {
    let rig = Rig::new().await;
    let (status, saved) = rig
        .post(
            "/native/providers/connections",
            json!({
                "id": "dead", "family": "ollama", "label": "dead",
                "endpoint": "http://127.0.0.1:9", "models": ["llama3:8b"],
                "allow_loopback": true, "connect_timeout_ms": 300,
                "total_timeout_ms": 600, "keep_alive": "30m"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["connect_timeout_ms"], 300);
    let (status, health) = rig
        .post(
            "/native/catalog/probe",
            json!({"connectionId": "dead", "model": "llama3:8b"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["state"], "failed");
    assert_eq!(
        health["certification"],
        "observed_reachability_and_model_list_only"
    );
    let (status, missing) = rig
        .post("/native/catalog/probe", json!({"connectionId": "absent"}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    let (status, unserved) = rig
        .post(
            "/native/catalog/probe",
            json!({"connectionId": "dead", "model": "not-declared"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{unserved}");
}

#[tokio::test]
async fn mcp_connect_on_a_stdio_record_is_a_typed_transport_mismatch() {
    let rig = Rig::new().await;
    let (status, saved) = rig
        .post(
            "/native/mcp/servers",
            json!({
                "id": "local", "name": "Local", "command": "npx",
                "args": ["-y", "some-mcp"], "env": [], "trusted": true,
                "enabled": true, "transport": "stdio"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let (status, body) = rig
        .post("/native/mcp/servers/local/connect", json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], json!("mcp_transport_mismatch"));
    assert!(!body.to_string().contains("mcp_remote_unsupported"));
}

#[tokio::test]
async fn mcp_auth_token_never_round_trips_and_bounds_are_typed() {
    let rig = Rig::new().await;
    let (status, saved) = rig
        .post(
            "/native/mcp/servers",
            json!({
                "id": "remote", "name": "Remote", "command": "", "args": [], "env": [],
                "trusted": true, "enabled": true, "transport": "streamable_http",
                "endpoint": "https://mcp.example.com/rpc", "timeout_ms": 5000,
                "output_cap_bytes": 1048576, "stderr_cap_bytes": 65536
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["record"]["auth_state"], "signed_out");
    let (status, auth) = rig
        .post(
            "/native/mcp/servers/remote/auth",
            json!({"action": "connect", "accessToken": "sk-secret-token",
                   "account": "acct", "scopes": ["tools.read"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{auth}");
    assert_eq!(auth["state"], "authorized");
    assert!(!auth.to_string().contains("sk-secret-token"));
    let (status, read) = rig.get("/native/mcp/servers/remote/auth").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["hasToken"], true);
    assert!(!read.to_string().contains("sk-secret-token"));
    let (status, servers) = rig.get("/native/mcp/servers").await;
    assert_eq!(status, StatusCode::OK, "{servers}");
    let remote = servers["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["record"]["id"] == "remote")
        .expect("saved remote server");
    assert_eq!(remote["remoteTransport"]["supported"], true);
    assert_eq!(remote["remoteTransport"]["configured"], true);
    assert_eq!(remote["record"]["transport"], "streamable_http");
    let (status, revoked) = rig
        .post("/native/mcp/servers/remote/oauth/revoke", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["state"], "signed_out");

    let (status, bad) = rig
        .post(
            "/native/mcp/servers",
            json!({
                "id": "bad", "name": "Bad", "command": "npx", "args": [], "env": [],
                "transport": "stdio", "timeout_ms": 1
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
}

#[tokio::test]
async fn review_comments_crud_is_scoped_and_revisioned() {
    let rig = Rig::new().await;
    let path = format!("/native/session/{}/review/comments", rig.session);
    let (status, created) = rig
        .post(
            &path,
            json!({"path": "src/lib.rs", "lineStart": 3, "lineEnd": 4,
                   "side": "new", "body": "guard this"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["comment"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["comment"]["sessionId"], rig.session);
    let (status, listed) = rig.get(&path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["comments"][0]["comment"]["body"], "guard this");
    let (status, patched) = rig
        .send(
            reqwest::Method::PATCH,
            &format!("{path}/{id}"),
            Some(json!({"resolved": true, "expectedRevision": 1})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["resolved"], true);
    let (status, _) = rig
        .send(
            reqwest::Method::PATCH,
            &format!("{path}/{id}"),
            Some(json!({"resolved": false, "expectedRevision": 1})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // A path traversal and a cross-session id are both refused.
    let (status, _) = rig
        .post(
            &path,
            json!({"path": "../escape", "lineStart": 1, "lineEnd": 1,
                   "side": "new", "body": "x"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let other = format!("/native/session/{}/review/comments", rig.session + 1000);
    let (status, _) = rig.get(&other).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, deleted) = rig
        .send(
            reqwest::Method::DELETE,
            &format!("{path}/{id}?expectedRevision=2"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, after) = rig.get(&path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["comments"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn session_settings_export_delete_and_change_accept_are_typed() {
    let rig = Rig::new().await;
    let settings = format!("/native/session/{}/settings", rig.session);
    let (status, updated) = rig
        .put(
            &settings,
            json!({"compactionModel": "fake/m", "maxTurns": 12,
                   "maxConsecutiveFailures": 3}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let (status, read) = rig.get(&settings).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["maxTurns"], 12);
    assert_eq!(read["compactionModel"], "fake/m");
    let (status, bad) = rig.put(&settings, json!({"maxTurns": 0})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    let (status, unserved) = rig
        .put(&settings, json!({"compactionModel": "nope/x"}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{unserved}");

    // No shadow is staged in this fixture: acceptance is a typed refusal,
    // never a fabricated landing.
    let accept = format!("/native/session/{}/changes/accept", rig.session);
    let (status, body) = rig
        .post(&accept, json!({"approved": ["a.txt"], "rejected": []}))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "unavailable");

    let export = format!("/native/session/{}/export", rig.session);
    let (status, bundle) = rig
        .post(&export, json!({"redact": true, "maxBytes": 65536}))
        .await;
    assert_eq!(status, StatusCode::OK, "{bundle}");
    assert_eq!(bundle["bundle"]["schema"], "faktor.session-export/v1");
    assert!(bundle["digest"].as_str().unwrap().len() == 64);

    let session = format!("/native/session/{}", rig.session);
    let (status, deleted) = rig.delete(&session).await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["idempotent"], false);
    let (status, replay) = rig.delete(&session).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["idempotent"], true);
    // Content is gone; a second export refuses with the tombstone.
    let (status, gone) = rig.post(&export, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{gone}");
}

#[tokio::test]
async fn restore_preview_is_wired_even_without_a_snapshot_authority() {
    let rig = Rig::new().await;
    let path = format!(
        "/native/session/{}/checkpoints/1/restore/preview",
        rig.session
    );
    let (status, body) = rig.post(&path, json!({"direction": "undo"})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"]["code"], "unavailable");
}

#[tokio::test]
async fn browser_validate_url_classifies_ipv6_aliases_and_never_echoes_secrets() {
    let rig = Rig::new().await;
    let validate = |url: &str| json!({ "url": url });

    // IPv6 literals (bracketed in the URL) are classified by the shared
    // authority, not skipped as "unresolved" by a v4-only parse.
    let (status, body) = rig
        .post(
            "/native/browser/validate-url",
            validate("http://[::1]:8080/x"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["class"], "loopback", "{body}");
    assert_eq!(body["decision"], "deny", "{body}");
    assert_eq!(body["url"], "http://[::1]:8080", "{body}");
    for alias in [
        "http://[::ffff:169.254.169.254]/latest/meta-data",
        "http://[64:ff9b::a9fe:a9fe]/x",
        "http://[2002:a9fe:a9fe::1]/x",
    ] {
        let (status, body) = rig
            .post("/native/browser/validate-url", validate(alias))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["decision"], "deny", "{alias}: {body}");
        assert_ne!(body["class"], "unresolved", "{alias}: {body}");
    }
    // Userinfo and secret-bearing path/query/fragment bytes never come back
    // in the preview payload.
    let (status, body) = rig
        .post(
            "/native/browser/validate-url",
            validate(
                "https://aliceUSER:p@ssPASSWORD@example.com/token/raw-secretPATH?api_key=supersecretQUERY#fragFRAGMENT",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let encoded = body.to_string();
    for leaked in [
        "aliceUSER",
        "p@ssPASSWORD",
        "raw-secretPATH",
        "supersecretQUERY",
        "fragFRAGMENT",
    ] {
        assert!(!encoded.contains(leaked), "{leaked} leaked: {encoded}");
    }
    let (status, body) = rig
        .post(
            "/native/browser/validate-url",
            validate(
                "https://example.com/token/raw-secretPATH?api_key=supersecretQUERY#fragFRAGMENT",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["url"], "https://example.com", "{body}");
    assert_eq!(body["queryPresent"], true, "{body}");
    assert_eq!(body["fragmentPresent"], true, "{body}");
    let encoded = body.to_string();
    for leaked in ["raw-secretPATH", "supersecretQUERY", "fragFRAGMENT"] {
        assert!(!encoded.contains(leaked), "{leaked} leaked: {encoded}");
    }
}

#[tokio::test]
async fn session_export_redacts_secret_patterns_in_titles_and_message_content() {
    let rig = Rig::new().await;
    // A recognizable planted credential in a user-controlled TITLE, a
    // message body and a message PART body: default redaction must remove
    // every occurrence, not only credential-shaped object keys.
    let secret = "sk-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    let ws = rig
        .deps
        .session
        .create_workspace("/tmp/export-redaction")
        .unwrap();
    let session = rig
        .deps
        .session
        .create_session(ws, &format!("title {secret}"), "fake", "m")
        .unwrap();
    let message = rig
        .deps
        .session
        .store()
        .put_message(
            session.id(),
            1,
            "user",
            json!({"text": format!("body {secret}")}),
        )
        .unwrap();
    rig.deps
        .session
        .store()
        .put_part(message, "text", json!({"text": format!("part {secret}")}))
        .unwrap();
    // A secret hidden in an object KEY (not just a value) must be redacted
    // by the recursive walk too.
    rig.deps
        .session
        .store()
        .put_part(message, "tool_result", json!({ secret: "value" }))
        .unwrap();

    let path = format!("/native/session/{}/export", session.id().raw());
    let (status, bundle) = rig.post(&path, json!({"redact": true})).await;
    assert_eq!(status, StatusCode::OK, "{bundle}");
    let encoded = bundle.to_string();
    assert!(
        !encoded.contains(secret),
        "default export redaction leaked a planted secret: {encoded}"
    );
    assert!(
        encoded.contains("<redacted:"),
        "the export must name what it redacted: {encoded}"
    );
    assert!(
        bundle["redactedFields"].as_u64().unwrap_or(0) >= 4,
        "title + message + part value + part key redactions are counted: {bundle}"
    );

    // `redact:false` is not an exfiltration switch: structured credential
    // keys that do not match a token pattern, and PEM private-key BODIES
    // (the pattern scan only names the header), are still scrubbed.
    let opaque_key = "opaque-custom-credential-value";
    let pem_body = "BODYBODYBODYBODYBODYBODYBODYBODY";
    rig.deps
        .session
        .store()
        .put_part(
            message,
            "tool",
            json!({
                "headers": {"ANTHROPIC_API_KEY": opaque_key},
                "pem": format!(
                    "-----BEGIN OPENSSH PRIVATE KEY-----\n{pem_body}\n-----END OPENSSH PRIVATE KEY-----"
                ),
            }),
        )
        .unwrap();
    let (status, bundle) = rig.post(&path, json!({"redact": false})).await;
    assert_eq!(status, StatusCode::OK, "{bundle}");
    let encoded = bundle.to_string();
    assert!(
        !encoded.contains(opaque_key),
        "credential key leaked: {encoded}"
    );
    assert!(
        !encoded.contains(pem_body),
        "private-key body leaked: {encoded}"
    );
    assert!(encoded.contains("<redacted:pem_private_key>"), "{encoded}");
    assert_eq!(bundle["redacted"], true, "{bundle}");
    assert_eq!(bundle["redactRequested"], false, "{bundle}");
}

#[test]
fn decision_paths_refuse_traversal_on_both_separators() {
    for hostile in [
        "",
        "../escape",
        "a/../../b",
        "a\\..\\b",
        "/absolute",
        "C:\\absolute",
        "\\\\server\\share",
        "a\0b",
    ] {
        assert!(
            crate::native::foundation_ext::agent_paths(&[hostile.to_string()]).is_err(),
            "decision path {hostile:?} must be refused"
        );
    }
    assert!(
        crate::native::foundation_ext::agent_paths(&["src/lib.rs".into()]).is_ok(),
        "workspace-relative paths stay accepted"
    );
}

/// F201/F202 regression over HTTP: the changes surface exposes line hunks and
/// the hunk-accept route rewrites only the selected composition, returns a
/// new candidate identity and marks prior verification stale.

#[tokio::test]
async fn changes_diff_and_per_hunk_acceptance_are_real_native_surfaces() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let shadows_dir = tempfile::tempdir().unwrap();
    let mut deps = crate::api::tests::test_deps(dir.path());
    let foundation = Arc::new(Foundation::open(dir.path().join("foundation.db")).unwrap());
    deps.foundation = Some(foundation);
    let manager = deps.session.clone();
    let agent = deps.agent.clone();
    let ws = manager
        .create_workspace(workspace.path().to_str().unwrap())
        .unwrap();
    let session = manager.create_session(ws, "hunks", "fake", "m").unwrap();
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(manager.clone(), agent.clone());
    let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
        manager.clone(),
        shadows_dir.path().join("shadows"),
    )
    .unwrap();
    deps.tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
        &orchestrator,
        manager,
        agent,
        shadows.clone(),
    );
    let token = deps.auth_token.clone();
    let handle = serve_arc(Arc::new(deps), 0).await.unwrap();
    let base = workspace.path().join("a.txt");
    std::fs::write(
        &base,
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
    )
    .unwrap();
    let session = session.id();
    let shadow = shadows
        .begin_shadow(session, workspace.path())
        .expect("shadow begins");
    std::fs::write(
        shadow.root.join("a.txt"),
        "one\nTWO SELECTED\nthree\nfour\nfive\nsix\nseven\neight\nNINE REJECTED\n",
    )
    .unwrap();
    let client = reqwest::Client::new();
    let base_url = format!("http://{}", handle.addr);
    let path = format!("/native/session/{}/changes", session.raw());
    let response = client
        .get(format!("{base_url}{path}"))
        .bearer_auth(token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let changes: Value = response.json().await.unwrap();
    assert_eq!(changes["files"][0]["path"], "a.txt", "{changes}");
    assert_eq!(
        changes["files"][0]["hunks"].as_array().unwrap().len(),
        2,
        "{changes}"
    );
    let identity = changes["candidateIdentity"].as_str().unwrap().to_string();
    let accept = format!("/native/session/{}/changes/accept-hunks", session.raw());
    let response = client
        .post(format!("{base_url}{accept}"))
        .bearer_auth(token.as_str())
        .json(&json!({
            "candidateIdentity": identity,
            "hunks": {"a.txt": [0]},
            "rejected": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let accepted: Value = response.json().await.unwrap();
    assert_eq!(
        accepted["proof"]["requiresReverification"], true,
        "{accepted}"
    );
    assert_eq!(accepted["integrated"], true, "{accepted}");
    let landed = std::fs::read_to_string(&base).unwrap();
    assert!(landed.contains("TWO SELECTED"), "{landed}");
    assert!(
        landed.contains("nine\n"),
        "unselected hunk stays base: {landed}"
    );
    assert!(!landed.contains("NINE REJECTED"), "{landed}");
    let response = client
        .post(format!("{base_url}{accept}"))
        .bearer_auth(token.as_str())
        .json(&json!({"candidateIdentity": identity, "hunks": {"a.txt": [0]}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let _ = handle.request_shutdown();
}
