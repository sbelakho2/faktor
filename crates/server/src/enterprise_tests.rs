//! Native enterprise route tests: typed disabled parity, principal gating,
//! the audit cursor export, settings ceilings, deletion jobs, the
//! effective-config refusal surface and the END-TO-END guarded GC over a
//! real session store + CAS (a live landing transaction protects its
//! rollback blob; the terminal phase releases it).

use super::tests::test_deps;
use super::*;
use faktor_cloud::{
    Action as CloudAction, ControlPlane, EnterpriseService, ManualClock, MemoryControlPlaneStore,
    Role as CloudRole,
};
use faktor_session::ledger::{
    EditTxnLedgerFile, IntegrationPathTxn, IntegrationPathTxnState, IntegrationTxnPhase,
    IntegrationTxnRow,
};
use faktor_session::{SessionHandle, SessionManager};

const NOW_MS: i64 = 1_700_000_000_000;

struct Env {
    handle: ServerHandle,
    daemon_token: String,
    owner_token: String,
    organization: String,
    control_plane: Arc<ControlPlane>,
    service: Arc<EnterpriseService>,
    session: Arc<SessionManager>,
    clock: Arc<ManualClock>,
}

fn digest(seed: u8) -> String {
    format!("{:02x}", seed).repeat(32)
}

fn open_txn(run: &str, rollback_hex: &str, phase: IntegrationTxnPhase) -> IntegrationTxnRow {
    IntegrationTxnRow {
        run_id: run.into(),
        task_id: 1,
        owner_root: "/owner".into(),
        candidate_root: "/candidate".into(),
        run_base_snapshot: digest(9),
        verified_candidate_snapshot: digest(9),
        sources_digest: digest(8),
        phase,
        paths: vec![IntegrationPathTxn {
            path: "src/lib.rs".into(),
            base_state: faktor_fs::entry_state::EntryState::Regular {
                mode: faktor_fs::tree_manifest::CanonicalMode::RegularFile,
                payload: faktor_core::hash::FileHash::from_hex(rollback_hex).unwrap(),
            },
            candidate_state: faktor_fs::entry_state::EntryState::Absent,
            rollback_blob: Some(rollback_hex.to_string()),
            rollback_link_target: None,
            canonical: true,
            state: IntegrationPathTxnState::Pending,
        }],
        path_count: 1,
        applied_count: 0,
        conflicts: Vec::new(),
        at_ms: 1,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wiring {
    /// No enterprise service at all (the disabled-parity shape).
    Disabled,
    /// The service wired WITHOUT the session/CAS retention runtime.
    Metadata,
    /// The service wired with the real reference scanner + guarded CAS.
    Full,
}

async fn enterprise_deps(root: &std::path::Path, wiring: Wiring) -> Env {
    let control_plane = Arc::new(ControlPlane::new(
        Arc::new(MemoryControlPlaneStore::new()),
        Arc::new(ManualClock::new(NOW_MS)),
    ));
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let service = Arc::new(
        EnterpriseService::new(
            Arc::new(MemoryControlPlaneStore::new()) as Arc<dyn faktor_cloud::EnterpriseStore>,
            clock.clone(),
        )
        .unwrap(),
    );
    let mut deps = test_deps(root).with_control_plane(control_plane.clone());
    let session = deps.session.clone();
    deps = match wiring {
        Wiring::Disabled => deps,
        Wiring::Metadata => deps.with_enterprise(service.clone(), None),
        Wiring::Full => deps.with_enterprise(
            service.clone(),
            Some(Arc::new(crate::native::enterprise::RetentionRuntime::new(
                session.store(),
                session.cas(),
            ))),
        ),
    };
    let token = deps.auth_token.clone();
    let handle = serve(deps, 0).await.unwrap();
    let client = reqwest::Client::new();
    let base = format!("http://{}", handle.addr);
    let (organization, owner_token) = bootstrap_owner(&client, &base, token.as_str()).await;
    Env {
        handle,
        daemon_token: token.as_str().to_string(),
        owner_token,
        organization,
        control_plane,
        service,
        session,
        clock,
    }
}

async fn bootstrap_owner(
    client: &reqwest::Client,
    base: &str,
    daemon_token: &str,
) -> (String, String) {
    let resp = client
        .post(format!("{base}/native/orgs"))
        .bearer_auth(daemon_token)
        .header("idempotency-key", "org-bootstrap")
        .json(&serde_json::json!({
            "name": "Acme", "owner_email": "owner@acme.test", "display_name": "Owner",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    (
        body["organization"]["id"].as_str().unwrap().to_string(),
        body["token"].as_str().unwrap().to_string(),
    )
}

fn url(env: &Env, path: &str) -> String {
    format!("http://{}{}", env.handle.addr, path)
}

#[tokio::test]
async fn enterprise_disabled_answers_typed_409() {
    let dir = tempfile::tempdir().unwrap();
    let env = enterprise_deps(dir.path(), Wiring::Disabled).await;
    let client = reqwest::Client::new();
    // The owner token authenticates against the control plane, but the
    // enterprise service itself is not wired: every route is a typed 409.
    let cases: [(&str, &str, serde_json::Value); 6] = [
        ("GET", "/native/enterprise/status", serde_json::Value::Null),
        (
            "GET",
            "/native/enterprise/audit?limit=10",
            serde_json::Value::Null,
        ),
        (
            "GET",
            "/native/enterprise/settings",
            serde_json::Value::Null,
        ),
        (
            "POST",
            "/native/enterprise/artifacts",
            serde_json::json!({
                "id": "art_1", "kind": "diagnostic_bundle", "digest": digest(1),
                "size": 1, "retention_class": "diagnostics", "ttl_ms": 60000,
            }),
        ),
        (
            "POST",
            "/native/enterprise/retention/gc",
            serde_json::json!({"limit": 10}),
        ),
        (
            "POST",
            "/native/enterprise/deletion-jobs",
            serde_json::json!({"scope": "organization"}),
        ),
    ];
    for (method, path, body) in cases {
        let mut request = match method {
            "GET" => client.get(url(&env, path)),
            _ => client.post(url(&env, path)),
        };
        request = request
            .bearer_auth(&env.daemon_token)
            .header("x-faktor-control-token", &env.owner_token)
            .header("idempotency-key", "k-1");
        if body != serde_json::Value::Null {
            request = request.json(&body);
        }
        let resp = request.send().await.unwrap();
        assert_eq!(resp.status(), 409, "{method} {path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "enterprise_disabled", "{path}");
    }
}

#[tokio::test]
async fn routes_require_a_control_plane_principal() {
    let dir = tempfile::tempdir().unwrap();
    let env = enterprise_deps(dir.path(), Wiring::Metadata).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(url(&env, "/native/enterprise/status"))
        .bearer_auth(&env.daemon_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // A garbage principal token is a typed 401 too.
    let resp = client
        .get(url(&env, "/native/enterprise/status"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", "not-a-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn guarded_gc_protects_a_live_transaction_blob_and_deletes_it_after_landing() {
    let dir = tempfile::tempdir().unwrap();
    let env = enterprise_deps(dir.path(), Wiring::Full).await;
    let client = reqwest::Client::new();

    // A real CAS blob a live landing transaction references.
    let bytes = b"authority-critical base payload";
    let hash = env.session.cas().put(bytes).unwrap();
    let hex = hash.to_hex();
    let workspace = env.session.create_workspace("/w").unwrap();
    let handle: SessionHandle = env
        .session
        .create_session(workspace, "task", "ollama", "qwen3.8")
        .unwrap();
    handle
        .ledger_integration_txn_set(&open_txn("run-gc", &hex, IntegrationTxnPhase::Prepared))
        .unwrap();

    // Register the artifact and let it expire.
    let resp = client
        .post(url(&env, "/native/enterprise/artifacts"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "art-1")
        .json(&serde_json::json!({
            "id": "art_rb",
            "session": "ses_1",
            "kind": "rollback_blob",
            "digest": hex,
            "size": bytes.len(),
            "retention_class": "authority_critical_rollback",
            "ttl_ms": 60000,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    env.clock.advance(120_000);

    let resp = client
        .post(url(&env, "/native/enterprise/artifacts/art_rb/eligible"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "eligible-1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["eligible"], true);

    // GC #1: the live transaction protects the blob (typed refusal).
    let resp = client
        .post(url(&env, "/native/enterprise/retention/gc"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "gc-1")
        .json(&serde_json::json!({"limit": 50}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["report"]["refused_protected"], 1, "{body}");
    assert_eq!(body["report"]["deleted"], 0, "{body}");
    assert!(env.session.cas().has(hash), "the protected blob is intact");
    assert_eq!(env.session.cas().get_verified_now(hash).unwrap(), bytes);

    // The transaction lands: the reference is released.
    handle
        .ledger_integration_txn_set(&open_txn("run-gc", &hex, IntegrationTxnPhase::Landed))
        .unwrap();
    let resp = client
        .post(url(&env, "/native/enterprise/retention/gc"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "gc-2")
        .json(&serde_json::json!({"limit": 50}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["report"]["deleted"], 1, "{body}");
    assert_eq!(body["ok"], true, "{body}");
    assert!(!env.session.cas().has(hash), "the released blob is deleted");

    // The ledger carries both decisions with digest refs.
    let resp = client
        .get(url(&env, "/native/enterprise/audit?limit=200"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let actions: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"retention_delete_refused"), "{actions:?}");
    assert!(actions.contains(&"retention_delete"), "{actions:?}");
}

#[tokio::test]
async fn role_gates_settings_audit_and_deletion_private_to_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let env = enterprise_deps(dir.path(), Wiring::Metadata).await;
    let client = reqwest::Client::new();

    // The owner principal creates a member-role service account.
    let owner = env.control_plane.authenticate(&env.owner_token).unwrap();
    let issued = env
        .control_plane
        .create_service_account(
            &owner,
            &owner.organization,
            "member-sa",
            CloudRole::Member,
            vec![CloudAction::RetentionRead, CloudAction::SettingsRead],
        )
        .unwrap();
    let member_token = issued.token.unwrap().expose().to_string();

    // Member: settings read is allowed, audit read and deletion are refused.
    let resp = client
        .get(url(&env, "/native/enterprise/settings"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &member_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .get(url(&env, "/native/enterprise/audit?limit=10"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &member_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let resp = client
        .post(url(&env, "/native/enterprise/deletion-jobs"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &member_token)
        .header("idempotency-key", "del-1")
        .json(&serde_json::json!({"scope": "organization"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Member cannot write settings either.
    let resp = client
        .put(url(&env, "/native/enterprise/settings"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &member_token)
        .header("idempotency-key", "set-1")
        .json(&serde_json::json!({"allowed_providers": ["anthropic"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);

    // Owner: write settings within the ceiling; an above-ceiling override
    // is refused.
    let resp = client
        .put(url(&env, "/native/enterprise/settings"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "set-2")
        .json(&serde_json::json!({
            "allowed_providers": ["anthropic"],
            "retention_overrides": {"provider_transcript": 86400000},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["settings"]["revision"], 1);

    let resp = client
        .put(url(&env, "/native/enterprise/settings"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "set-3")
        .json(&serde_json::json!({
            "retention_overrides": {"provider_transcript": 31536000000_i64},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "above the class ceiling");

    // Strict DTO: an unknown key is a 400.
    let resp = client
        .put(url(&env, "/native/enterprise/settings"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "set-4")
        .json(&serde_json::json!({"surprise": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn deletion_job_walks_all_steps_and_retains_billing_rows() {
    let dir = tempfile::tempdir().unwrap();
    let env = enterprise_deps(dir.path(), Wiring::Full).await;
    let client = reqwest::Client::new();
    let auth = |request: reqwest::RequestBuilder, key: &str| {
        request
            .bearer_auth(&env.daemon_token)
            .header("x-faktor-control-token", &env.owner_token)
            .header("idempotency-key", key)
    };
    // One ordinary artifact and one billing record.
    for (id, class, seed) in [
        ("art_ord", "diagnostics", 3u8),
        ("art_bill", "billing_record", 4u8),
    ] {
        let resp = auth(
            client.post(url(&env, "/native/enterprise/artifacts")),
            &format!("{id}-key"),
        )
        .json(&serde_json::json!({
            "id": id, "kind": "diagnostic_bundle", "digest": digest(seed),
            "size": 10, "retention_class": class,
            "ttl_ms": if class == "billing_record" { serde_json::Value::Null } else { serde_json::json!(60000) },
        }))
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), 201, "{id}");
    }

    let resp = auth(
        client.post(url(&env, "/native/enterprise/deletion-jobs")),
        "job-1",
    )
    .json(&serde_json::json!({"scope": "organization"}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 201);
    let body: serde_json::Value = resp.json().await.unwrap();
    let job_id = body["job"]["id"].as_str().unwrap().to_string();

    let mut state = String::new();
    for step in 0..4 {
        let resp = auth(
            client.post(url(
                &env,
                &format!("/native/enterprise/deletion-jobs/{job_id}/advance"),
            )),
            &format!("advance-{step}"),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), 200, "step {step}");
        let body: serde_json::Value = resp.json().await.unwrap();
        state = body["job"]["state"].as_str().unwrap().to_string();
        if state == "completed" {
            break;
        }
    }
    assert_eq!(state, "completed");

    let resp = auth(
        client.get(url(&env, "/native/enterprise/artifacts?limit=10")),
        "list-1",
    )
    .send()
    .await
    .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let states: std::collections::BTreeMap<String, String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["id"].as_str().unwrap().to_string(),
                item["deletion_state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(states.get("art_ord").unwrap(), "deleted");
    assert_eq!(states.get("art_bill").unwrap(), "retained");

    let scope_key = format!("org:{}", env.organization);
    let resp = auth(
        client.get(url(
            &env,
            &format!("/native/enterprise/tombstones/{scope_key}"),
        )),
        "tomb-1",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["tombstone"]["deleted_artifacts"], 1);
    assert_eq!(body["tombstone"]["retained_artifacts"], 1);

    // An unknown tombstone is a typed 404.
    let resp = auth(
        client.get(url(&env, "/native/enterprise/tombstones/org:nope")),
        "tomb-2",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn effective_config_refuses_policy_loosening_and_changes_digest_with_any_layer() {
    let dir = tempfile::tempdir().unwrap();
    let env = enterprise_deps(dir.path(), Wiring::Metadata).await;
    let client = reqwest::Client::new();
    let layers = |network: &[&str], task_network: Option<&[&str]>| {
        let mut layers = vec![serde_json::json!({
            "scope": "organization",
            "semantics": "policy",
            "revision": 1,
            "values": {"network": {"kind": "policy", "value": network}},
        })];
        if let Some(task_network) = task_network {
            layers.push(serde_json::json!({
                "scope": "task",
                "semantics": "policy",
                "revision": 1,
                "values": {"network": {"kind": "policy", "value": task_network}},
            }));
        }
        serde_json::json!({ "layers": layers })
    };

    let resp = client
        .post(url(&env, "/native/enterprise/effective-config"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .json(&layers(&["none"], None))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let first_digest = body["digest"].as_str().unwrap().to_string();
    assert_eq!(body["attestation"]["digest"], first_digest);

    // A task override that widens the org deny is a typed 409.
    let resp = client
        .post(url(&env, "/native/enterprise/effective-config"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .json(&layers(&["none"], Some(&["none", "provider"])))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "policy_refused");

    // Changing the layer changes the digest.
    let resp = client
        .post(url(&env, "/native/enterprise/effective-config"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .json(&layers(&["none", "mcp"], None))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_ne!(body["digest"].as_str().unwrap(), first_digest);
}

#[tokio::test]
async fn register_artifact_strictness_and_edit_txn_protection() {
    let dir = tempfile::tempdir().unwrap();
    let env = enterprise_deps(dir.path(), Wiring::Full).await;
    let client = reqwest::Client::new();

    // A malformed digest is refused before any durable write.
    let resp = client
        .post(url(&env, "/native/enterprise/artifacts"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "bad-1")
        .json(&serde_json::json!({
            "id": "art_bad", "kind": "diagnostic_bundle", "digest": "not-a-digest",
            "size": 1, "retention_class": "diagnostics", "ttl_ms": 60000,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    // Unknown retention class / kind are 400s.
    for (kind, class) in [
        ("nonsense", "diagnostics"),
        ("diagnostic_bundle", "nonsense"),
    ] {
        let resp = client
            .post(url(&env, "/native/enterprise/artifacts"))
            .bearer_auth(&env.daemon_token)
            .header("x-faktor-control-token", &env.owner_token)
            .header("idempotency-key", &format!("bad-{kind}-{class}"))
            .json(&serde_json::json!({
                "id": "art_bad2", "kind": kind, "digest": digest(1),
                "size": 1, "retention_class": class, "ttl_ms": 60000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{kind}/{class}");
    }

    // An OPEN edit transaction protects its staged base content from GC.
    let staged = env.session.cas().put(b"staged base content").unwrap();
    let staged_hex = staged.to_hex();
    let workspace = env.session.create_workspace("/w2").unwrap();
    let handle = env
        .session
        .create_session(workspace, "task", "ollama", "qwen3.8")
        .unwrap();
    handle
        .ledger_edit_txn_prepared(
            7,
            "ses",
            &[EditTxnLedgerFile {
                path: "src/lib.rs".into(),
                base_digest: staged_hex.clone(),
                base_bytes_len: 19,
            }],
            "roll_forward",
        )
        .unwrap();
    let resp = client
        .post(url(&env, "/native/enterprise/artifacts"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "staged-1")
        .json(&serde_json::json!({
            "id": "art_staged", "kind": "source_excerpt", "digest": staged_hex,
            "size": 19, "retention_class": "source_excerpt", "ttl_ms": 60000,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    env.clock.advance(120_000);
    let resp = client
        .post(url(
            &env,
            "/native/enterprise/artifacts/art_staged/eligible",
        ))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "staged-2")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = client
        .post(url(&env, "/native/enterprise/retention/gc"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "staged-3")
        .json(&serde_json::json!({"limit": 50}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["report"]["refused_protected"], 1, "{body}");
    assert!(env.session.cas().has(staged));

    // Committing the edit transaction releases it.
    handle
        .ledger_edit_txn_committed(7, &["src/lib.rs".into()], &[], &[])
        .unwrap();
    let resp = client
        .post(url(&env, "/native/enterprise/retention/gc"))
        .bearer_auth(&env.daemon_token)
        .header("x-faktor-control-token", &env.owner_token)
        .header("idempotency-key", "staged-4")
        .json(&serde_json::json!({"limit": 50}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["report"]["deleted"], 1, "{body}");
    assert!(!env.session.cas().has(staged));
    // The service is still alive and reachable after the pass: one artifact
    // row remains, now in the deleted tombstone state.
    let organization = faktor_cloud::OrganizationId::try_new(env.organization.clone()).unwrap();
    let rows = env
        .service
        .store()
        .artifacts(&organization, None, 10)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].deletion_state, faktor_cloud::DeletionState::Deleted);
}
