//! Production-wiring and adversarial tests for the Revision 8 daemon
//! contract routes. Every test starts the real native listener and speaks
//! HTTP with the daemon password; nothing is called through a private seam.

#![allow(clippy::too_many_lines)]

use std::sync::Arc;

use faktor_foundation::context::{Basis, ContextStore, Manifest, Source};
use faktor_foundation::{BrowserAdapterDraft, BrowserMode, Change, Foundation, Patch, Scope};
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::api::tests::test_deps;
use crate::api::{serve_arc, ServerDeps, ServerHandle};

pub(crate) struct Rig {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) deps: Arc<ServerDeps>,
    pub(crate) foundation: Arc<Foundation>,
    pub(crate) _handle: ServerHandle,
    pub(crate) base: String,
    pub(crate) session: u64,
}

impl Rig {
    pub(crate) async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut deps = test_deps(dir.path());
        let foundation = Arc::new(Foundation::open(dir.path().join("foundation.db")).unwrap());
        deps.foundation = Some(foundation.clone());
        let ws = deps
            .session
            .create_workspace(&dir.path().to_string_lossy())
            .unwrap();
        let session = deps
            .session
            .create_session(ws, "r8-native", "fake", "m")
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

    pub(crate) async fn send(
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

    pub(crate) async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::GET, path, None).await
    }
    pub(crate) async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
    pub(crate) async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }
    pub(crate) async fn delete(&self, path: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::DELETE, path, None).await
    }
}

#[tokio::test]
async fn default_provider_and_model_settings_apply_to_new_sessions() {
    let rig = Rig::new().await;
    let (status, snapshot) = rig.get("/native/config/snapshot").await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    let revision = snapshot["revision"].as_u64().expect("revision");
    let (status, receipt) = rig
        .post(
            "/native/config/commit",
            json!({
                "expected_revision": revision,
                "changes": [
                    {"key": "faktor.defaultProvider", "scope": "user", "value": "fake"},
                    {"key": "faktor.defaultModel", "scope": "user", "value": "m"},
                ],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    // Empty per-session values resolve the durable defaults; this is the
    // "new conversation" timing the settings screen advertises.
    let (status, created) = rig
        .post(
            "/native/session",
            json!({
                "provider": "",
                "model": "",
                "workspace": rig.dir.path().to_string_lossy(),
                "title": "defaults",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let (status, sessions) = rig.get("/native/sessions").await;
    assert_eq!(status, StatusCode::OK);
    let row = sessions["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == created["id"])
        .expect("the new session is listed");
    assert_eq!(row["provider"], "fake");
    assert_eq!(row["model"], "m");
}

#[tokio::test]
async fn instruction_records_preview_binding_and_import_are_wired() {
    let rig = Rig::new().await;
    let (status, record) = rig
        .post(
            "/native/instructions/records",
            json!({
                "kind": "instruction", "title": "Style", "content": "use tabs",
                "origin": "user", "scope": "user", "activation": "always"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    let id = record["id"].as_str().unwrap().to_string();
    assert_eq!(record["version"], 1);

    let (status, preview) = rig
        .post(
            "/native/instructions/records/preview",
            json!({"prompt": "fix", "touchedPaths": [], "selectedIds": []}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(preview["content"].as_str().unwrap().contains("use tabs"));
    assert_eq!(preview["estimate"]["method"], "byte-quarter-estimator");

    let body = json!({"session": rig.session.to_string(), "operationId": "op-1"});
    let (status, first) = rig
        .post("/native/instructions/bindings", body.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["revision"], 1);
    let (status, replay) = rig.post("/native/instructions/bindings", body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["revision"], 1, "replay is idempotent");

    // A project-scope row with the same title wins deterministically.
    let (status, _) = rig
        .post(
            "/native/instructions/records",
            json!({
                "kind": "instruction", "title": "Style", "content": "use spaces",
                "origin": "project", "scope": "project", "activation": "always"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, preview) = rig
        .post(
            "/native/instructions/records/preview",
            json!({"prompt": "", "touchedPaths": [], "selectedIds": []}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(preview["content"].as_str().unwrap().contains("use spaces"));
    assert!(!preview["content"].as_str().unwrap().contains("use tabs"));
    assert!(!preview["conflicts"][0]["winner"]
        .as_str()
        .unwrap()
        .is_empty());

    // A different captured bundle under the same operation refuses.
    let (status, conflict) = rig
        .post(
            "/native/instructions/bindings",
            json!({"session": rig.session.to_string(), "operationId": "op-1"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");

    let (status, report) = rig
        .post(
            "/native/instructions/import",
            json!({"format": "claude/v1", "bundle": {"mcpServers": {}, "instructions": []}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(report["imported"], 0);
    assert!(report["unsupported"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "mcpServers"));

    let (status, list) = rig
        .get("/native/instructions/records?kind=instruction&limit=1")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["records"].as_array().unwrap().len(), 1);

    // Hostile bodies and unbounded content never reach the store.
    let (status, _) = rig
        .post(
            "/native/instructions/records",
            json!({"kind": "instruction", "title": "x", "content": "y", "extra": true}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig
        .post(
            "/native/instructions/records",
            json!({"kind": "instruction", "title": "x", "content": "y".repeat(300_000)}),
        )
        .await;
    assert!(status.is_client_error());
    let (status, _) = rig
        .delete(&format!(
            "/native/instructions/records/{id}?expectedRevision=1"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = rig
        .delete(&format!(
            "/native/instructions/records/{id}?expectedRevision=1"
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn preset_envelopes_bind_before_dispatch_and_refuse_read_only_mutation() {
    let rig = Rig::new().await;
    let (status, ask) = rig
        .post(
            "/native/presets",
            json!({
                "mode": "ask", "name": "Ask", "origin": "user", "scope": "user",
                "toolAllow": ["read", "mcp:not-installed"], "toolDeny": ["run"],
                "budgetSharePpm": 500000
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ask}");
    let ask_id = ask["id"].as_str().unwrap().to_string();
    let (status, envelope) = rig.get(&format!("/native/presets/{ask_id}/envelope")).await;
    assert_eq!(status, StatusCode::OK, "{envelope}");
    assert_eq!(envelope["admissionMutationAllowed"], false);
    assert!(envelope["tools"]["resolved"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| { t["name"] == "mcp:not-installed" && t["offered"] == false }));

    let mutating = json!({
        "goal": "change code", "submission_id": "11111111-1111-1111-1111-111111111111",
        "preset_id": ask_id,
        "work_items": [{"id": "w1", "kind": "Implementation"}]
    });
    let (status, refused) = rig
        .post(
            &format!("/native/session/{}/task-runs", rig.session),
            mutating.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"]["code"], "read_only_preset");

    let mut plain = mutating.clone();
    plain.as_object_mut().unwrap().remove("work_items");
    let (status, refused) = rig
        .post(&format!("/native/session/{}/task-runs", rig.session), plain)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"]["code"], "read_only_preset_needs_plan");

    let (status, code) = rig
        .post(
            "/native/presets",
            json!({"mode": "code", "name": "Code", "origin": "user", "scope": "user"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{code}");
    let code_id = code["id"].as_str().unwrap().to_string();
    // This start fails plan validation AFTER the envelope is durably bound;
    // the binding must still be readable and replay-proof.
    let (status, failed) = rig
        .post(
            &format!("/native/session/{}/task-runs", rig.session),
            json!({
                "goal": "change code", "submission_id": "22222222-2222-2222-2222-222222222222",
                "preset_id": code_id,
                "work_items": [{"id": "w1", "kind": "Implementation", "ownership": "no_writes"}]
            }),
        )
        .await;
    assert!(
        status.is_client_error(),
        "mutating work with NoWrites must fail: {failed}"
    );
    let (status, binding) = rig
        .get(&format!(
            "/native/presets/binding?session={}&submissionId=22222222-2222-2222-2222-222222222222",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{binding}");
    assert_eq!(binding["presetId"], code_id);
    assert_eq!(binding["envelope"]["mode"], "code");

    // Same submission, a different preset: typed conflict, never a rewrite.
    let (status, _) = rig
        .post(
            &format!("/native/session/{}/task-runs", rig.session),
            json!({
                "goal": "again", "submission_id": "22222222-2222-2222-2222-222222222222",
                "preset_id": ask_id,
                "work_items": [{"id": "w1", "kind": "Analysis"}]
            }),
        )
        .await;
    assert!(status.is_client_error());

    // A foreign actor cannot write a managed preset.
    let (status, _) = rig
        .post(
            "/native/presets",
            json!({"mode": "ask", "name": "Managed", "origin": "managed", "scope": "managed"}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn runtime_trust_is_exact_path_and_never_claims_signature_verification() {
    let rig = Rig::new().await;
    let exe = rig.dir.path().join("faktor-daemon");
    std::fs::write(&exe, b"v1").unwrap();
    let path = exe.to_string_lossy().to_string();
    let (status, record) = rig
        .post(
            "/native/runtime/trust",
            json!({"executablePath": path, "publisher": "Release"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{record}");
    assert_eq!(record["signatureState"], "unverified");
    assert_eq!(record["signatureBasis"], "no_code_signing_verifier_wired");

    let (status, check) = rig
        .post(
            "/native/runtime/trust/check",
            json!({"executablePath": path}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["state"], "match");
    assert_eq!(check["startAllowed"], true);
    assert_eq!(check["signatureState"], "unverified");

    std::fs::write(&exe, b"v2").unwrap();
    let (_, check) = rig
        .post(
            "/native/runtime/trust/check",
            json!({"executablePath": path}),
        )
        .await;
    assert_eq!(check["state"], "mismatch");
    assert_eq!(check["startAllowed"], false);

    let (status, _) = rig
        .post(
            "/native/runtime/trust",
            json!({"executablePath": "relative/path"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, status_body) = rig.get("/native/runtime/status").await;
    assert_eq!(status, StatusCode::OK, "{status_body}");
    assert_eq!(status_body["protocol"], "v1");
    assert!(status_body["shellIsolation"]["mode"].is_string());
    assert!(status_body["startedMs"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn browser_lifecycle_refuses_without_adapter_and_bounds_captures() {
    let rig = Rig::new().await;
    let (status, adapters) = rig.get("/native/browser/adapters").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(adapters["advertised"], false);
    assert_eq!(adapters["state"], "unavailable");

    let (status, refused) = rig
        .post(
            "/native/browser/sessions",
            json!({"adapterId": "chromium", "session": rig.session.to_string(), "url": "https://example.com"}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");

    // The host wiring registers an adapter; HTTP can never advertise one.
    rig.foundation
        .browser
        .upsert_adapter(&BrowserAdapterDraft {
            id: "chromium".into(),
            mode: BrowserMode::Local,
            endpoint: Some("http://127.0.0.1:9222".into()),
            account_scope: Some("daemon".into()),
            advertised: true,
            network_scope: "loopback_only".into(),
        })
        .unwrap();
    let (status, session) = rig
        .post(
            "/native/browser/sessions",
            json!({"adapterId": "chromium", "session": rig.session.to_string(), "url": "https://example.com/app"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["state"], "requested");
    let browser_session = session["id"].as_str().unwrap().to_string();

    // Adapter events are the only way an observation exists.
    rig.foundation
        .browser
        .append_observation(
            &browser_session,
            "console",
            Some("error"),
            &json!({"message": "boom"}),
        )
        .unwrap();
    let (status, observations) = rig
        .get(&format!(
            "/native/browser/sessions/{browser_session}/observations?severity=error"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(observations["count"], 1);
    assert_eq!(
        observations["observations"][0]["payload"]["message"],
        "boom"
    );

    let (status, _) = rig
        .post(
            &format!("/native/browser/sessions/{browser_session}/screenshot"),
            json!({"mime": "image/png", "dataBase64": "not base64!"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let png = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        b"\x89PNG\r\n\x1a\n",
    );
    let (status, artifact) = rig
        .post(
            &format!("/native/browser/sessions/{browser_session}/screenshot"),
            json!({"mime": "image/png", "dataBase64": png, "inModelContext": true}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{artifact}");
    assert_eq!(artifact["inModelContext"], true);
    let id = artifact["id"].as_str().unwrap();
    let response = rig
        .client()
        .get(format!("{}/native/browser/artifacts/{id}/bytes", rig.base))
        .bearer_auth(rig.deps.auth_token.as_str())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        b"\x89PNG\r\n\x1a\n"
    );

    for url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "https://user:pw@x.test/",
    ] {
        let (status, _) = rig
            .post(
                "/native/browser/sessions",
                json!({"adapterId": "chromium", "session": rig.session.to_string(), "url": url}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url} must be refused");
    }
}

#[tokio::test]
async fn context_pins_export_config_provenance_and_reset_are_wired() {
    let rig = Rig::new().await;
    let store = ContextStore::new(rig.foundation.db.clone());
    store
        .capture(Manifest {
            session_id: rig.session,
            revision: 0,
            operation_id: "op".into(),
            call_id: None,
            model: "m".into(),
            capacity: 32_768,
            input: Some(10),
            output_reserve: 4_096,
            safety_reserve: 2_048,
            basis: Basis::Estimated {
                method: "test".into(),
                version: "1".into(),
            },
            sources: vec![Source {
                id: "src-0".into(),
                uri: "file:///w/a.rs".into(),
                range: Some("1-2".into()),
                version: "v1".into(),
                content_hash: "h".into(),
                category: "files".into(),
                tokens: Some(10),
                bytes: Some(5),
                inclusion_reason: "explicit".into(),
            }],
            created_ms: 0,
        })
        .unwrap();
    let (status, pin) = rig
        .post(
            &format!("/native/session/{}/context/pins", rig.session),
            json!({"sourceId": "src-0", "action": "pin"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pin}");
    let (status, pins) = rig
        .get(&format!("/native/session/{}/context/pins", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pins["pins"].as_array().unwrap().len(), 1);
    let (status, export) = rig
        .get(&format!("/native/context/export?session={}", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK, "{export}");
    assert_eq!(export["sources"][0]["pinned"], true);
    assert_eq!(export["manifest"]["capacity"], 32_768);
    let (status, removed) = rig
        .delete(&format!(
            "/native/session/{}/context/pins?sourceId=src-0",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(removed["removed"], true);

    // Config provenance: user then project, deterministic winner + conflict.
    let mut rev = rig.foundation.config.snapshot().unwrap().revision;
    for (scope, value) in [(Scope::User, 2), (Scope::Project, 3)] {
        rig.foundation
            .config
            .commit(&Patch {
                expected_revision: rev,
                changes: vec![Change {
                    key: "faktor.retry.maxAttempts".into(),
                    scope,
                    value: Some(json!(value)),
                }],
            })
            .unwrap();
        rev = rig.foundation.config.snapshot().unwrap().revision;
    }
    let (status, provenance) = rig
        .get("/native/config/provenance?q=faktor.retry.maxAttempts")
        .await;
    assert_eq!(status, StatusCode::OK, "{provenance}");
    let row = &provenance["rows"][0];
    assert_eq!(row["effective"]["scope"], "project");
    assert_eq!(row["effective"]["value"], 3);
    assert!(provenance["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| { c["key"] == "faktor.retry.maxAttempts" && c["winner"] == "project" }));

    let (status, reset) = rig
        .post(
            "/native/config/reset",
            json!({"expectedRevision": rev, "key": "faktor.retry.maxAttempts", "scope": "project"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reset}");
    let snapshot = rig.foundation.config.snapshot().unwrap();
    assert_eq!(
        snapshot.values["faktor.retry.maxAttempts"].scope,
        Scope::User
    );
}

#[tokio::test]
async fn efficiency_and_budget_surfaces_are_measured_or_unavailable_never_fake() {
    let rig = Rig::new().await;
    let (status, efficiency) = rig
        .get(&format!("/native/session/{}/efficiency", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK, "{efficiency}");
    assert_eq!(efficiency["cache"]["state"], "unavailable");
    assert_eq!(efficiency["routing"]["outcomes"]["state"], "unavailable");
    assert_eq!(efficiency["localOperatingCost"]["state"], "unavailable");
    assert!(efficiency["compaction"].is_array());

    let path = format!("/native/session/{}/budget?taskId=1", rig.session);
    let (status, budget) = rig.get(&path).await;
    assert_eq!(status, StatusCode::OK, "{budget}");
    assert!(matches!(
        budget["state"].as_str(),
        Some("known") | Some("unavailable")
    ));
    if budget["state"] == "unavailable" {
        assert!(budget["reason"].is_string());
    }

    let (status, _) = rig
        .put(
            &format!("/native/session/{}/budget", rig.session),
            json!({"taskId": "1", "maxCostMicro": "1.25", "clear": true}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = rig
        .put(
            &format!("/native/session/{}/budget", rig.session),
            json!({"taskId": "1", "maxCostMicro": "not-money"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // No route can fabricate a connected browser or a verified signature.
    let (status, _) = rig.get("/native/runtime/trust?path=/missing/binary").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn edition5_foundation_surfaces_are_wired_over_http() {
    let rig = Rig::new().await;
    let (status, descriptors) = rig.get("/native/config/descriptors").await;
    assert_eq!(status, StatusCode::OK, "{descriptors}");
    assert!(!descriptors["descriptors"].as_array().unwrap().is_empty());
    let (status, snapshot) = rig.get("/native/config/snapshot").await;
    assert_eq!(status, StatusCode::OK);
    let revision = snapshot["revision"].as_u64().unwrap();

    let patch = json!({
        "expected_revision": revision,
        "changes": [{"key": "faktor.retry.maxAttempts", "scope": "user", "value": 3}]
    });
    let (status, preview) = rig.post("/native/config/preview", patch.clone()).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    // A preview carries the validated DIFF (key/scope/current/next/timing),
    // not merely a count: the Settings screen renders it before committing.
    assert_eq!(preview["changes"][0]["key"], "faktor.retry.maxAttempts");
    assert_eq!(preview["changes"][0]["scope"], "user");
    assert_eq!(preview["changes"][0]["next"], 3);
    assert!(preview["changes"][0]["applyTiming"].is_string());
    assert_eq!(preview["affectedScopes"], json!(["user"]));
    let (status, receipt) = rig.post("/native/config/commit", patch).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["changed"], 1);
    let (status, search) = rig.get("/native/config/search?q=retry&limit=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(search["hits"].as_array().unwrap().len() <= 5);
    let (status, export) = rig.get("/native/config/export").await;
    assert_eq!(status, StatusCode::OK);
    assert!(export["omitted_secrets"].is_array());

    let (status, compile) = rig
        .post(
            "/native/model-parameters/compile",
            json!({"provider": "fake", "model": "m", "parameters": {"temperature": 0.0}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{compile}");
    assert_eq!(compile["parameters"]["temperature"]["value"], 0.0);
    let (status, unknown) = rig
        .post(
            "/native/model-parameters/compile",
            json!({"provider": "fake", "model": "m", "parameters": {"hardBudegt": 1}}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{unknown}");

    let manifest = json!({
        "operationId": "op-wire",
        "capacity": 32768,
        "input": 100,
        "outputReserve": 4096,
        "safetyReserve": 2048,
        "sources": [{
            "id": "s0", "uri": "file:///w/a.rs", "range": "1-2", "version": "v1",
            "content_hash": "h", "category": "files", "tokens": 100,
            "bytes": 10, "inclusion_reason": "explicit"
        }]
    });
    let (status, preview) = rig
        .post(
            &format!("/native/session/{}/context/preview", rig.session),
            manifest.clone(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let (status, captured) = rig
        .post(
            &format!("/native/session/{}/context/manifests", rig.session),
            manifest,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{captured}");
    let (status, latest) = rig
        .get(&format!("/native/context/manifest?session={}", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(latest["manifest"]["revision"], 1);

    let (status, policy) = rig.get("/native/permissions/policy").await;
    assert_eq!(status, StatusCode::OK);
    assert!(policy["revision"].is_number());
    let (status, _audit) = rig.get("/native/permissions/audit?limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let (status, verdict) = rig
        .post(
            "/native/permissions/preview",
            json!({"tool_id": "read", "canonical_target": "file:///w/a.rs", "effect": "read"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{verdict}");
    assert!(verdict["decision"].is_string());

    let (status, tools) = rig.get("/native/tools?q=read&limit=5").await;
    assert_eq!(status, StatusCode::OK);
    assert!(tools["tools"].is_array());
    let (status, _skills) = rig.get("/native/skills?q=a").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _hooks) = rig.get("/native/hooks?q=a&limit=5").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _mcp) = rig.get("/native/mcp/servers?q=a&limit=5").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _memory) = rig
        .get(&format!("/native/memory?session={}&limit=10", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, parallel) = rig.get("/native/sessions/parallel").await;
    assert_eq!(status, StatusCode::OK);
    assert!(parallel["sessions"].is_array());

    let (status, diag) = rig.get("/native/diagnostics/preview").await;
    assert_eq!(status, StatusCode::OK, "{diag}");
    assert!(diag["maxBytes"].as_u64().unwrap() > 0);
    let (status, bundle) = rig.post("/native/diagnostics/export", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{bundle}");
    assert!(bundle["bytes"].as_u64().unwrap() > 0);
    assert!(bundle["checksum"].as_str().unwrap().len() >= 32);
    assert!(bundle["content"]["started_ms"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn fork_restore_and_partial_acceptance_are_typed() {
    let rig = Rig::new().await;
    let (status, fork) = rig
        .post(
            &format!("/native/session/{}/fork", rig.session),
            json!({"title": "forked"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{fork}");
    assert_eq!(fork["forkedFrom"], rig.session.to_string());
    assert_ne!(fork["sessionId"], rig.session.to_string());
    let forked = fork["sessionId"].as_str().unwrap();
    let (status, _forked_turns) = rig.get(&format!("/native/session/{forked}/turns")).await;
    assert_eq!(status, StatusCode::OK);

    // The test host wires no snapshot/fs authority: restore must say so,
    // never pretend to have restored content.
    let (status, restore) = rig
        .post(
            &format!("/native/session/{}/checkpoints/1/restore", rig.session),
            json!({"direction": "undo"}),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{restore}");
    let (status, _bad_direction) = rig
        .post(
            &format!("/native/session/{forked}/checkpoints/1/restore",),
            json!({"direction": "sideways"}),
        )
        .await;
    assert!(status.is_client_error() || status == StatusCode::SERVICE_UNAVAILABLE);

    // Partial acceptance: an unknown child or a hostile body is typed and
    // writes nothing.
    let (status, _unknown_child) = rig
        .post("/native/agents/child-does-not-exist/stage", json!({}))
        .await;
    assert!(status.is_client_error() || status.is_server_error());
    let (status, _bad_merge) = rig
        .post(
            "/native/agents/child-does-not-exist/merge",
            json!({"changeSetId": "", "approved": [], "rejected": []}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unauthenticated_requests_are_refused_for_every_r8_surface() {
    let rig = Rig::new().await;
    for path in [
        "/native/instructions/records",
        "/native/presets",
        "/native/runtime/status",
        "/native/browser/adapters",
        "/native/context/export?session=1",
        "/native/config/provenance",
    ] {
        let response = rig
            .client()
            .get(format!("{}{}", rig.base, path))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

/// Report row 18: the skills listing is cursor-paged. All 600 discovered
/// skills are reachable; the old fixed `.take(500)` left 501+ invisible.
#[tokio::test]
async fn skills_listing_cursor_reaches_every_discovered_skill() {
    let rig = Rig::new().await;
    for i in 0..600 {
        let dir = rig.dir.path().join(format!(".faktor/skills/skill-{i:04}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("# Skill {i}\n## Keywords: kw-{i}\nbody {i}\n"),
        )
        .unwrap();
    }
    let mut names: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 10, "the cursor walk must terminate");
        let path = match &after {
            Some(cursor) => format!(
                "/native/skills?session={}&limit=200&after={}",
                rig.session,
                urlencoding(cursor)
            ),
            None => format!("/native/skills?session={}&limit=200", rig.session),
        };
        let (status, body) = rig.get(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["discoveredTotal"], 600);
        for skill in body["skills"].as_array().expect("skills array") {
            if skill["installed"] == false {
                names.push(skill["name"].as_str().expect("name").to_string());
            }
        }
        if body["hasMore"] == true {
            after = Some(
                body["nextCursor"]
                    .as_str()
                    .expect("nextCursor with hasMore")
                    .to_string(),
            );
        } else {
            break;
        }
    }
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 600, "every discovered skill is reachable");
    // The cursor is four pages over one unchanged tree: exactly ONE full
    // discovery may run (fingerprint hits for every later page).
    let root = rig.dir.path().canonicalize().unwrap();
    assert_eq!(
        crate::native::foundation_ext::skill_discoveries_for(&root),
        1,
        "the cursor walk must not re-run discovery per page"
    );
    // Exactness: a changed SKILL.md invalidates the fingerprint and the next
    // page reflects the new summary (no stale cache).
    std::fs::write(
        rig.dir.path().join(".faktor/skills/skill-0000/SKILL.md"),
        "# Changed summary with a different size and body\n",
    )
    .unwrap();
    let (status, changed) = rig
        .get(&format!("/native/skills?session={}", rig.session))
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let changed_skill = changed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|skill| skill["name"] == "skill-0000")
        .expect("changed skill still listed");
    assert_eq!(
        changed_skill["description"], "Changed summary with a different size and body",
        "the cache must not mask a changed skill: {changed_skill}"
    );
    assert_eq!(
        crate::native::foundation_ext::skill_discoveries_for(&root),
        2,
        "a changed tree must re-discover exactly once"
    );

    fn urlencoding(value: &str) -> String {
        value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                    c.to_string()
                } else {
                    let mut buf = [0u8; 4];
                    c.encode_utf8(&mut buf)
                        .bytes()
                        .map(|b| format!("%{b:02X}"))
                        .collect()
                }
            })
            .collect()
    }
}

#[tokio::test]
async fn telemetry_payload_preview_is_explicit_and_never_claims_upload() {
    let rig = Rig::new().await;
    let (status, body) = rig.get("/native/telemetry").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], json!(false));
    assert_eq!(body["state"], json!("disabled"));
    assert_eq!(body["endpoint"], Value::Null);
    assert!(body["endpointScope"]
        .as_str()
        .unwrap()
        .contains("no upload endpoint"));
    let excluded = body["payloadPreview"]["excluded"].as_array().unwrap();
    for expected in ["secrets", "credentials", "prompt text", "source code"] {
        assert!(
            excluded.iter().any(|entry| entry == expected),
            "payload preview must exclude {expected:?}: {body}"
        );
    }
    assert!(body["payloadPreview"]["bounded"].as_bool().unwrap());
}

#[tokio::test]
async fn native_model_aliases_require_the_native_prefix() {
    let rig = Rig::new().await;
    for path in ["/native/models", "/native/capabilities"] {
        let (status, body) = rig.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
    }
    let (status, body) = rig.get("/models").await;
    assert_eq!(status, StatusCode::OK, "legacy alias stays served: {body}");
}

#[tokio::test]
async fn mcp_resources_for_an_unknown_server_is_typed_not_a_catalog() {
    let rig = Rig::new().await;
    let (status, body) = rig.get("/native/mcp/servers/missing/resources").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], json!("not_found"));
}

#[tokio::test]
async fn lsp_routes_are_typed_when_no_language_server_manager_is_wired() {
    let rig = Rig::new().await;
    let (status, body) = rig
        .get(&format!(
            "/native/lsp/diagnostics?session={}&path=src/lib.rs",
            rig.session
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "lsp_disabled");
    let (status, body) = rig
        .post(
            "/native/lsp/code-actions",
            json!({
                "session": rig.session.to_string(),
                "path": "src/lib.rs",
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1}
                }
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "lsp_disabled");
}
