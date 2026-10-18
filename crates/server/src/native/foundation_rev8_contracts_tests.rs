//! Production-wiring tests for the Rev8 field-level daemon contracts. Every
//! test speaks HTTP to the real native listener; nothing uses a private
//! handler seam. Adversarial cases cover hostile bodies, foreign scopes,
//! idempotent replay and typed refusals instead of fake ready states.

use reqwest::StatusCode;
use serde_json::json;

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
