// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Rev8 long-task acceptance probes: S08/S10/S11/S12/S62/S66/S69.
//!
//! These drive the REAL native routes (auth, DTOs, typed errors). They are
//! deliberately adversarial where the spec is: stale revisions, deleted
//! memories, instruction hash races, cancel-after-terminal, restart reads.

use std::sync::Arc;

use crate::api::tests::*;
use crate::api::*;
use axum::http::StatusCode;
use faktor_foundation::context::{Basis, Manifest, Source};
use faktor_foundation::Foundation;
use serde_json::{json, Value};

struct Rig {
    dir: tempfile::TempDir,
    deps: Arc<ServerDeps>,
    foundation: Arc<Foundation>,
    handle: ServerHandle,
    base: String,
    session: u64,
}

impl Rig {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // The real-roots rig mirrors the daemon graph: instruction loading
        // resolves the session's durable workspace root, not a test stub.
        let mut rig = native_task_rig(
            dir.path(),
            vec![vec![faktor_provider::ScriptedResponse::End]],
            false,
            false,
        );
        let foundation = Arc::new(Foundation::open(dir.path().join("foundation.db")).unwrap());
        rig.deps.foundation = Some(foundation.clone());
        let deps = Arc::new(rig.deps);
        let handle = serve_arc(deps.clone(), 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        Self {
            dir,
            deps,
            foundation,
            handle,
            base,
            session: rig.parent.raw(),
        }
    }

    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
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
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    fn handle(&self) -> faktor_session::SessionHandle {
        self.deps
            .session
            .get_session(faktor_core::id::SessionId::new(self.session))
            .unwrap()
            .unwrap()
    }

    async fn capture_usage(&self, input: u64, tokens: u64) {
        self.foundation
            .context
            .capture(Manifest {
                session_id: self.session,
                revision: 0,
                operation_id: "op-usage".into(),
                call_id: Some("call-usage".into()),
                model: "m".into(),
                capacity: 32_768,
                input: Some(input),
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
                    tokens: Some(tokens),
                    bytes: Some(5),
                    inclusion_reason: "explicit".into(),
                }],
                created_ms: 0,
            })
            .unwrap();
    }
}

/// A11/A12: the projection reports `contextUsage = null` until an
/// authoritative manifest exists, then the EXACT checked arithmetic. S62's
/// "Unavailable" is never rendered as a fake zero.
#[tokio::test]
async fn acceptance_a11_a12_context_usage_null_then_authoritative() {
    let rig = Rig::new().await;
    let (status, body) = rig
        .send(
            reqwest::Method::GET,
            &format!("/session/{}/projection", rig.session),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert!(body["contextUsage"].is_null(), "{body}");
    assert!(body["compaction"].is_null(), "{body}");
    assert_eq!(body["longTask"]["compacting"], false);

    rig.capture_usage(4_000, 4_000).await;
    let (status, body) = rig
        .send(
            reqwest::Method::GET,
            &format!("/session/{}/projection", rig.session),
            None,
        )
        .await;
    assert_eq!(status, 200);
    let usage = &body["contextUsage"];
    assert_eq!(usage["capacity"], 32_768);
    assert_eq!(usage["input"], 4_000);
    assert_eq!(usage["estimated"], true);
    assert_eq!(usage["measured"], false);
    assert_eq!(
        usage["headroom"],
        32_768 - 4_000 - 4_096 - 2_048,
        "headroom is the checked arithmetic, never a guess: {body}"
    );
    let _ = rig.handle.request_shutdown();
}

/// A15 + S10/K-03/K-04: an explicit compaction operation is durable,
/// post-inspection survives, a stale expected revision refuses, and a cancel
/// after the terminal state returns the exact terminal row (no rewrite).
#[tokio::test]
async fn acceptance_a15_long_task_compaction_operation_contract() {
    let rig = Rig::new().await;
    let handle = rig.handle();
    for turn in 0..40 {
        let seq = handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::ReadyForNextTurn,
                None,
                None,
            )
            .unwrap();
        let message = handle
            .put_message(
                seq.raw() as i64,
                if turn % 2 == 0 { "user" } else { "assistant" },
                json!({"parts": []}),
            )
            .unwrap();
        handle
            .put_text_part(message, &format!("TURN-{turn:03} {}", "h".repeat(3_000)))
            .unwrap();
    }
    rig.capture_usage(4_000, 4_000).await;
    let path = format!("/native/session/{}/context/compaction", rig.session);
    let (status, started) = rig
        .send(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "operationId": "op-accept-1",
                "targetTokens": 4_000,
                "minReductionRatio": 0.25,
                "expectedRevision": 1
            })),
        )
        .await;
    assert_eq!(status, 200, "{started}");
    assert_eq!(started["operationId"], "op-accept-1");
    assert_eq!(started["state"], "applied");
    assert_eq!(started["terminal"], true);
    assert_eq!(started["progress"], 1.0);
    assert_eq!(started["expectedRevision"], 1);
    assert_eq!(started["basis"]["kind"], "estimated");
    assert!(
        started["result"]["summaryRevision"].as_i64().unwrap() > 0,
        "post-compaction inspection cites the durable summary revision: {started}"
    );
    assert!(started["result"]["reduction"].as_f64().unwrap() >= 0.25);
    // Normal success converges registration: the executor guard removed the
    // key before the response was produced (no explicit unregister path).
    assert!(
        !super::operation_is_active(handle.id(), "op-accept-1"),
        "a completed executor must not remain registered"
    );
    // The replacement context is the NEWEST suffix, not the store's
    // newest-first row order: the active user prompt must survive and the
    // oldest turn must be archived.
    let kept: Vec<faktor_context::RecentTurn> =
        serde_json::from_slice(&handle.context_checkpoint().unwrap().unwrap().payload).unwrap();
    assert!(
        kept.iter().any(|turn| turn.text.contains("TURN-038")),
        "the active user prompt must ride the compacted wire: {kept:?}"
    );
    assert!(
        !kept.iter().any(|turn| turn.text.contains("TURN-000")),
        "the oldest turn must be archived, not kept as a 'newest' suffix: {kept:?}"
    );
    // Duplicate op id is idempotent: the same durable outcome, no re-run.
    let (status, replay) = rig
        .send(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "operationId": "op-accept-1",
                "targetTokens": 4_000,
                "minReductionRatio": 0.25,
                "expectedRevision": 1
            })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(replay["state"], "applied");
    assert_eq!(replay["result"], started["result"]);
    // A SECOND compaction loads the checkpoint replacement plus only rows
    // STRICTLY newer than its high-water seq, in oldest-first order. The
    // newest post-checkpoint prompt must survive; the pre-checkpoint turn
    // must not resurface as a "newest" suffix.
    let before_second = handle.context_checkpoint().unwrap().unwrap();
    for turn in 0..6 {
        let seq = handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::ReadyForNextTurn,
                None,
                None,
            )
            .unwrap();
        let message = handle
            .put_message(seq.raw() as i64, "user", json!({"parts": []}))
            .unwrap();
        handle
            .put_text_part(
                message,
                &format!("POST-CHECKPOINT-{turn:03} {}", "k".repeat(2_000)),
            )
            .unwrap();
    }
    let (status, second) = rig
        .send(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "operationId": "op-accept-2",
                "targetTokens": 4_000,
                "minReductionRatio": 0.25
            })),
        )
        .await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["state"], "applied", "{second}");
    let after_second = handle.context_checkpoint().unwrap().unwrap();
    assert!(
        after_second.through_seq > before_second.through_seq,
        "the second compaction must advance the checkpoint high-water seq"
    );
    let kept_second: Vec<faktor_context::RecentTurn> =
        serde_json::from_slice(&after_second.payload).unwrap();
    assert!(
        kept_second
            .iter()
            .any(|turn| turn.text.contains("POST-CHECKPOINT-005")),
        "the newest post-checkpoint prompt must ride the wire: {kept_second:?}"
    );
    assert!(
        !kept_second
            .iter()
            .any(|turn| turn.text.contains("TURN-000")),
        "pre-checkpoint rows must not be replayed: {kept_second:?}"
    );
    // Stale expected revision is a typed conflict.
    let (status, stale) = rig
        .send(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "operationId": "op-stale",
                "targetTokens": 4_000,
                "expectedRevision": 0
            })),
        )
        .await;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["error"]["code"], "context_revision_conflict");
    // Status and latest read back the exact durable operation.
    let (status, status_body) = rig
        .send(reqwest::Method::GET, &format!("{path}/op-accept-1"), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(status_body["state"], "applied");
    assert_eq!(status_body["result"], started["result"]);
    let (status, latest) = rig.send(reqwest::Method::GET, &path, None).await;
    assert_eq!(status, 200);
    assert_eq!(latest["operationId"], "op-accept-2");
    // A cancel AFTER the terminal outcome reports the durable terminal state
    // and never rewrites it.
    let (status, cancelled) = rig
        .send(
            reqwest::Method::POST,
            &format!("{path}/op-accept-1/cancel"),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(cancelled["cancelRequested"], false);
    assert_eq!(cancelled["operation"]["state"], "applied");
    // A prepared (not yet started) op has NO executor: cancelling it must
    // finalize the durable state now, never leave a zombie "compacting".
    handle
        .begin_compaction_operation("op-prepared", 0, 0.25, None, None)
        .unwrap();
    let (status, cancelled) = rig
        .send(
            reqwest::Method::POST,
            &format!("{path}/op-prepared/cancel"),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(cancelled["cancelRequested"], true);
    assert_eq!(cancelled["operation"]["state"], "cancelled");
    assert_eq!(cancelled["operation"]["terminal"], true);
    // The projection exposes the latest operation and no stale "compacting".
    let (_, projection) = rig
        .send(
            reqwest::Method::GET,
            &format!("/session/{}/projection", rig.session),
            None,
        )
        .await;
    assert_eq!(
        projection["longTask"]["compacting"], false,
        "a finalized cancel is not running: {projection}"
    );
    assert_eq!(projection["compaction"]["operationId"], "op-prepared");
    assert_eq!(projection["compaction"]["state"], "cancelled");

    // A target the pruner can never meet is REFUSED (the ~1% failure path),
    // never labelled "Compacted": the prior checkpoint stays authoritative.
    let before = handle
        .context_checkpoint()
        .unwrap()
        .expect("the accepted compaction persisted a checkpoint");
    let (status, refused) = rig
        .send(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "operationId": "op-refuse",
                "targetTokens": 1,
                "minReductionRatio": 0.25
            })),
        )
        .await;
    assert_eq!(status, 200, "{refused}");
    assert_eq!(refused["state"], "refused");
    assert_eq!(refused["terminal"], true);
    assert_eq!(refused["result"]["accepted"], false);
    assert!(
        !super::operation_is_active(handle.id(), "op-refuse"),
        "a refused (error-shaped) terminal outcome must not remain registered"
    );
    let rejected = refused["result"]["rejectedReason"].as_str().unwrap();
    assert!(
        rejected.contains("target") || rejected.contains("compactor rejected"),
        "a refusal names the unmet invariant: {rejected}"
    );
    let after = handle.context_checkpoint().unwrap().unwrap();
    assert_eq!(
        after.digest, before.digest,
        "a refused compaction must not advance the checkpoint"
    );
    assert_eq!(after.through_seq, before.through_seq);

    // Crash residue: a non-terminal row from a PREVIOUS process has no live
    // executor. Reading it must not paint a spinner forever; `committing`
    // is the one phase whose effect is genuinely unknown.
    let now_op = |operation_id: &str, state: &str| faktor_store::CompactionOperationRow {
        session_id: handle.id(),
        operation_id: operation_id.into(),
        state: state.into(),
        phase: state.into(),
        progress_ppm: 500_000,
        expected_revision: None,
        manifest_revision: None,
        before_tokens: None,
        after_tokens: None,
        target_tokens: 10,
        min_reduction_ppm: 250_000,
        strategy: None,
        result_json: None,
        error_code: None,
        error_message: None,
        created_ms: 1,
        updated_ms: 0,
    };
    rig.deps
        .session
        .store()
        .insert_compaction_operation(now_op("crash-reducing", "reducing"))
        .unwrap();
    rig.deps
        .session
        .store()
        .insert_compaction_operation(now_op("crash-committing", "committing"))
        .unwrap();
    for (operation, expected) in [
        ("crash-reducing", "cancelled"),
        ("crash-committing", "effect_unknown"),
    ] {
        let (status, row) = rig
            .send(reqwest::Method::GET, &format!("{path}/{operation}"), None)
            .await;
        assert_eq!(status, 200, "{row}");
        assert_eq!(row["state"], expected, "{row}");
        assert_eq!(row["terminal"], true);
    }
    let (_, committing) = rig
        .send(
            reqwest::Method::GET,
            &format!("{path}/crash-committing"),
            None,
        )
        .await;
    assert_eq!(committing["error"]["code"], "effect_unknown");
    let _ = rig.handle.request_shutdown();
}

/// A16/S12/S66/K-10/K-11: memory create/dedupe/conflict/update/delete/
/// restore with provenance, workspace isolation, typed errors and the
/// exact deletion receipt.
#[tokio::test]
async fn acceptance_a16_memory_records_crud_and_workspace_scope() {
    let rig = Rig::new().await;
    let path = "/native/memory/records";
    let (status, created) = rig
        .send(
            reqwest::Method::POST,
            path,
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "workspace",
                "subject": "project",
                "predicate": "test.command",
                "value": {"type": "text", "value": "cargo test"},
                "provenance": {"origin": "user", "sourceRef": "settings"},
                "confidencePpm": 1000000
            })),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["created"], true);
    let id = created["record"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["record"]["version"], 1);
    assert_eq!(created["record"]["scope"], "workspace");
    assert_eq!(created["record"]["provenance"]["origin"], "user");
    assert_eq!(created["record"]["provenance"]["sourceRef"], "settings");
    assert!(created["record"]["deleted"].is_null());
    // Exact duplicate is a no-op; drift is a typed conflict.
    let (status, duplicate) = rig
        .send(
            reqwest::Method::POST,
            path,
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "workspace",
                "subject": "project",
                "predicate": "test.command",
                "value": {"type": "text", "value": "cargo test"}
            })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(duplicate["deduped"], true);
    assert_eq!(duplicate["record"]["version"], 1);
    let (status, conflict) = rig
        .send(
            reqwest::Method::POST,
            path,
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "workspace",
                "subject": "project",
                "predicate": "test.command",
                "value": {"type": "text", "value": "cargo nextest"}
            })),
        )
        .await;
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["error"]["code"], "memory_conflict");
    // Versioned edit; a stale version refuses without a write.
    let (status, updated) = rig
        .send(
            reqwest::Method::PUT,
            &format!("{path}/{id}"),
            Some(json!({
                "session": rig.session.to_string(),
                "expectedVersion": 1,
                "value": {"type": "text", "value": "cargo test --workspace"}
            })),
        )
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["record"]["version"], 2);
    assert_eq!(updated["record"]["previous"]["version"], 1);
    let (status, stale) = rig
        .send(
            reqwest::Method::PUT,
            &format!("{path}/{id}"),
            Some(json!({
                "session": rig.session.to_string(),
                "expectedVersion": 1,
                "value": {"type": "text", "value": "stale"}
            })),
        )
        .await;
    assert_eq!(status, 409);
    assert_eq!(stale["error"]["code"], "memory_version_conflict");
    // Delete: exact S66 receipt; the tombstone remains visible only with
    // includeDeleted=true.
    let (status, deleted) = rig
        .send(
            reqwest::Method::DELETE,
            &format!(
                "{path}/{id}?session={}&expectedVersion=2&reason=user%20request",
                rig.session
            ),
            None,
        )
        .await;
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["receipt"]["retainedPriorCopies"], true);
    assert!(
        deleted["receipt"]["futureUseStoppedAtRevision"].is_u64(),
        "the receipt names a durable revision: {deleted}"
    );
    assert!(
        deleted["receipt"]["deletedValueHash"]
            .as_str()
            .unwrap()
            .len()
            == 64
    );
    let (_, live) = rig
        .send(
            reqwest::Method::GET,
            &format!(
                "{path}?session={}&scope=workspace&includeDeleted=false",
                rig.session
            ),
            None,
        )
        .await;
    assert!(
        live["records"].as_array().unwrap().is_empty(),
        "a deleted memory is never listed for retrieval: {live}"
    );
    let (_, deleted_page) = rig
        .send(
            reqwest::Method::GET,
            &format!(
                "{path}?session={}&scope=workspace&includeDeleted=true",
                rig.session
            ),
            None,
        )
        .await;
    assert_eq!(deleted_page["records"][0]["id"], id);
    assert!(deleted_page["records"][0]["deleted"]["atRevision"].is_u64());
    // Restore revives it.
    let (status, restored) = rig
        .send(
            reqwest::Method::POST,
            &format!("{path}/{id}/restore"),
            Some(json!({
                "session": rig.session.to_string(),
                "expectedVersion": 3
            })),
        )
        .await;
    assert_eq!(status, 200, "{restored}");
    assert_eq!(restored["record"]["version"], 4);
    assert!(restored["record"]["deleted"].is_null());
    // A sibling session of the SAME workspace sees the shared project
    // memory (S12 scope semantics), while a session scope stays private.
    let parent_ws = rig.handle().row().unwrap().workspace_id;
    let sibling = rig
        .deps
        .session
        .create_session(parent_ws, "sibling", "fake", "m")
        .unwrap();
    let (status, sibling_page) = rig
        .send(
            reqwest::Method::GET,
            &format!(
                "{path}?session={}&scope=workspace&includeDeleted=false",
                sibling.id()
            ),
            None,
        )
        .await;
    assert_eq!(status, 200, "{sibling_page}");
    assert_eq!(
        sibling_page["records"][0]["id"], id,
        "workspace memory is shared by every session of the workspace: {sibling_page}"
    );
    assert_eq!(sibling_page["records"][0]["version"], 4);
    let private = rig
        .send(
            reqwest::Method::POST,
            path,
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "session",
                "subject": "private",
                "predicate": "only-here",
                "value": {"type": "text", "value": "secret"}
            })),
        )
        .await
        .1;
    assert_eq!(private["created"], true);
    let (_, sibling_private) = rig
        .send(
            reqwest::Method::GET,
            &format!(
                "{path}?session={}&scope=session&includeDeleted=true",
                sibling.id()
            ),
            None,
        )
        .await;
    assert!(
        sibling_private["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|record| record["id"] != private["record"]["id"]),
        "session memory stays private: {sibling_private}"
    );
    // Another workspace cannot read this scope even with the literal id.
    let other_ws = rig.deps.session.create_workspace("/other").unwrap();
    let other = rig
        .deps
        .session
        .create_session(other_ws, "other", "fake", "m")
        .unwrap();
    let (_, other_page) = rig
        .send(
            reqwest::Method::GET,
            &format!(
                "{path}?session={}&scope=workspace&includeDeleted=true",
                other.id()
            ),
            None,
        )
        .await;
    assert!(
        other_page["records"].as_array().unwrap().is_empty(),
        "workspace memory is not cross-project: {other_page}"
    );
    let _ = rig.handle.request_shutdown();
}

/// S12/O-09 policy + retention: stale policy revisions conflict, the dry-run
/// lists exactly the tombstone the apply pass then deletes; live records are
/// never candidates.
#[tokio::test]
async fn acceptance_s12_memory_policy_and_retention() {
    let rig = Rig::new().await;
    let policy_path = "/native/memory/policy";
    let (status, policy) = rig
        .send(
            reqwest::Method::GET,
            &format!("{policy_path}?session={}", rig.session),
            None,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(policy["policy"]["revision"], 0);
    assert_eq!(policy["policy"]["auto_capture"], "manual");
    assert_eq!(policy["failureLearning"]["separate"], true);
    let (status, saved) = rig
        .send(
            reqwest::Method::PUT,
            policy_path,
            Some(json!({
                "session": rig.session.to_string(),
                "expectedRevision": 0,
                "policy": {
                    "revision": 0,
                    "auto_capture": "controlled",
                    "retention_days": 30,
                    "max_records": 5000,
                    "allow_session_scope": true,
                    "allow_workspace_scope": true
                }
            })),
        )
        .await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["policy"]["revision"], 1);
    assert_eq!(saved["policy"]["auto_capture"], "controlled");
    let (status, stale) = rig
        .send(
            reqwest::Method::PUT,
            policy_path,
            Some(json!({
                "session": rig.session.to_string(),
                "expectedRevision": 0,
                "policy": {
                    "auto_capture": "off",
                    "retention_days": 1,
                    "max_records": 10
                }
            })),
        )
        .await;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["error"]["code"], "memory_conflict");

    // One tombstone + one live record; only the tombstone may be retained.
    let created = rig
        .send(
            reqwest::Method::POST,
            "/native/memory/records",
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "session",
                "subject": "p",
                "predicate": "old",
                "value": {"type": "text", "value": "old"}
            })),
        )
        .await
        .1;
    let old_id = created["record"]["id"].as_str().unwrap().to_string();
    rig.send(
        reqwest::Method::POST,
        "/native/memory/records",
        Some(json!({
            "session": rig.session.to_string(),
            "scope": "session",
            "subject": "p",
            "predicate": "live",
            "value": {"type": "text", "value": "live"}
        })),
    )
    .await;
    rig.send(
        reqwest::Method::DELETE,
        &format!(
            "/native/memory/records/{old_id}?session={}&expectedVersion=1",
            rig.session
        ),
        None,
    )
    .await;
    // Time-travel the tombstone into the past by writing it back through the
    // store seam (the API clock is real time): a retention cutoff of 1ms is
    // enough for a just-deleted record only after this tiny durable update.
    let handle = rig.handle();
    let row = rig
        .deps
        .session
        .store()
        .memory_fact_by_key(handle.id(), &old_id)
        .unwrap()
        .unwrap();
    let mut fact: faktor_memory::MemoryFactV2 = serde_json::from_str(&row.value).unwrap();
    fact.deleted.as_mut().unwrap().deleted_ms = 1;
    fact.created_ms = 1;
    fact.updated_ms = 2;
    rig.deps
        .session
        .store()
        .upsert_memory_fact(
            handle.id(),
            &fact.row_kind(),
            &fact.id,
            &serde_json::to_string(&fact).unwrap(),
        )
        .unwrap();
    let (status, dry) = rig
        .send(
            reqwest::Method::POST,
            "/native/memory/records/retention",
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "session",
                "olderThanDays": 0,
                "dryRun": true
            })),
        )
        .await;
    assert_eq!(status, 200, "{dry}");
    assert_eq!(dry["wouldDelete"], 1, "{dry}");
    assert_eq!(dry["candidates"][0]["id"], old_id);
    let (status, applied) = rig
        .send(
            reqwest::Method::POST,
            "/native/memory/records/retention",
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "session",
                "olderThanDays": 0,
                "dryRun": false
            })),
        )
        .await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["deleted"], 1);
    assert!(rig
        .deps
        .session
        .store()
        .memory_fact_by_key(handle.id(), &old_id)
        .unwrap()
        .is_none());

    // max_records is a REAL bound for new records (not a decorative field):
    // the limit refuses a genuinely new record with a typed error, while an
    // identical duplicate (a no-op) still succeeds.
    let (status, limited) = rig
        .send(
            reqwest::Method::PUT,
            policy_path,
            Some(json!({
                "session": rig.session.to_string(),
                "expectedRevision": 1,
                "policy": {
                    "auto_capture": "controlled",
                    "retention_days": 30,
                    "max_records": 1,
                    "allow_session_scope": true,
                    "allow_workspace_scope": true
                }
            })),
        )
        .await;
    assert_eq!(status, 200, "{limited}");
    let first_ws = rig
        .send(
            reqwest::Method::POST,
            "/native/memory/records",
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "workspace",
                "subject": "limit",
                "predicate": "one",
                "value": {"type": "text", "value": "v1"}
            })),
        )
        .await;
    assert_eq!(first_ws.0, 200, "{:?}", first_ws.1);
    let dup_ws = rig
        .send(
            reqwest::Method::POST,
            "/native/memory/records",
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "workspace",
                "subject": "limit",
                "predicate": "one",
                "value": {"type": "text", "value": "v1"}
            })),
        )
        .await;
    assert_eq!(dup_ws.0, 200, "dedupe is not a new record: {:?}", dup_ws.1);
    assert_eq!(dup_ws.1["deduped"], true);
    let over = rig
        .send(
            reqwest::Method::POST,
            "/native/memory/records",
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "workspace",
                "subject": "limit",
                "predicate": "two",
                "value": {"type": "text", "value": "v2"}
            })),
        )
        .await;
    assert_eq!(over.0, 409, "{:?}", over.1);
    assert_eq!(over.1["error"]["code"], "memory_policy_limit");
    assert_eq!(over.1["error"]["maxRecords"], 1);
    let raised = rig
        .send(
            reqwest::Method::PUT,
            policy_path,
            Some(json!({
                "session": rig.session.to_string(),
                "expectedRevision": 2,
                "policy": {
                    "auto_capture": "controlled",
                    "retention_days": 30,
                    "max_records": 2,
                    "allow_session_scope": true,
                    "allow_workspace_scope": true
                }
            })),
        )
        .await;
    assert_eq!(raised.0, 200, "{:?}", raised.1);
    let allowed = rig
        .send(
            reqwest::Method::POST,
            "/native/memory/records",
            Some(json!({
                "session": rig.session.to_string(),
                "scope": "workspace",
                "subject": "limit",
                "predicate": "two",
                "value": {"type": "text", "value": "v2"}
            })),
        )
        .await;
    assert_eq!(allowed.0, 200, "{:?}", allowed.1);
    let _ = rig.handle.request_shutdown();
}

/// S11/S69/I-11: source list/precedence/conflict, bounded merged preview,
/// hash-checked create/update, and next-task vs restart binding.
#[tokio::test]
async fn acceptance_s11_s69_instructions_list_preview_and_edit() {
    let rig = Rig::new().await;
    let rules = rig.dir.path().join("owner/.faktor/rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::write(
        rules.join("style.md"),
        "---\n# Scope: src/**\n---\nPrefer small functions.\n",
    )
    .unwrap();
    std::fs::write(
        rig.dir.path().join("owner/AGENTS.md"),
        "Run the workspace tests before reporting.\n",
    )
    .unwrap();
    let list_path = format!("/native/instructions?session={}", rig.session);
    let (status, list) = rig.send(reqwest::Method::GET, &list_path, None).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["precedence"][0]["source"], "faktor_native");
    assert_eq!(list["binding"]["activeRun"], false);
    assert_eq!(list["binding"]["applyTiming"], "next_task");
    let paths: Vec<&str> = list["instructions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    assert!(paths.iter().any(|p| p.ends_with("AGENTS.md")), "{list}");
    assert!(
        paths.iter().any(|p| p.ends_with(".faktor/rules/style.md")),
        "{list}"
    );
    // I-11: a driven turn captures the current epoch; the next task sees a
    // matching binding, and a later edit changes the epoch without rewriting
    // the captured one.
    rig.deps
        .agent
        .run_turn(
            faktor_core::id::SessionId::new(rig.session),
            "capture the bundle",
            &[],
        )
        .await
        .unwrap();
    let (_, bound) = rig.send(reqwest::Method::GET, &list_path, None).await;
    assert_eq!(bound["binding"]["matches"], true, "{bound}");
    assert_eq!(bound["binding"]["capturedEpoch"], bound["epoch"]);
    assert_eq!(
        bound["binding"]["capturedBundleHash"], bound["binding"]["currentBundleHash"],
        "the captured bundle hash is the inspector's own material hash: {bound}"
    );
    let (status, preview) = rig
        .send(
            reqwest::Method::GET,
            &format!(
                "/native/instructions/preview?session={}&prompt=edit%20src/lib.rs",
                rig.session
            ),
            None,
        )
        .await;
    assert_eq!(status, 200, "{preview}");
    assert!(preview["text"]
        .as_str()
        .unwrap()
        .contains("Prefer small functions."));
    assert!(preview["bytes"].as_u64().unwrap() <= 64 * 1024);
    // Create a new native rule atomically; then a stale update conflicts.
    let (status, created) = rig
        .send(
            reqwest::Method::PUT,
            "/native/instructions/source",
            Some(json!({
                "session": rig.session.to_string(),
                "path": ".faktor/rules/new.md",
                "content": "Always name the failing test.\n",
                "mode": "create"
            })),
        )
        .await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["changed"], true);
    assert_eq!(created["applyTiming"], "next_task");
    let content_hash = created["contentHash"].as_str().unwrap().to_string();
    let (status, stale) = rig
        .send(
            reqwest::Method::PUT,
            "/native/instructions/source",
            Some(json!({
                "session": rig.session.to_string(),
                "path": ".faktor/rules/new.md",
                "content": "Never name the failing test.\n",
                "mode": "update",
                "expectedHash": "0".repeat(64)
            })),
        )
        .await;
    assert_eq!(status, 409, "{stale}");
    assert_eq!(stale["error"]["code"], "instruction_source_conflict");
    let (status, updated) = rig
        .send(
            reqwest::Method::PUT,
            "/native/instructions/source",
            Some(json!({
                "session": rig.session.to_string(),
                "path": ".faktor/rules/new.md",
                "content": "Always name the failing test and its log.\n",
                "mode": "update",
                "expectedHash": content_hash
            })),
        )
        .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["changed"], true);
    assert!(updated["epoch"].is_string());
    let (_, rebound) = rig.send(reqwest::Method::GET, &list_path, None).await;
    assert_eq!(
        rebound["binding"]["matches"], false,
        "an edit changes the live epoch but not the captured binding: {rebound}"
    );
    assert_eq!(
        rebound["binding"]["capturedBundleHash"], bound["binding"]["currentBundleHash"],
        "an edit never rewrites the captured bundle hash: {rebound}"
    );
    assert_ne!(
        rebound["binding"]["currentBundleHash"],
        bound["binding"]["currentBundleHash"]
    );
    assert_eq!(rebound["binding"]["applyTiming"], "next_task");
    // Hostile path / oversized content are typed refusals, never writes.
    let (status, traversal) = rig
        .send(
            reqwest::Method::PUT,
            "/native/instructions/source",
            Some(json!({
                "session": rig.session.to_string(),
                "path": "../escape.md",
                "content": "x",
                "mode": "create"
            })),
        )
        .await;
    assert_eq!(status, 400, "{traversal}");
    let (status, outside) = rig
        .send(
            reqwest::Method::PUT,
            "/native/instructions/source",
            Some(json!({
                "session": rig.session.to_string(),
                "path": "src/evil.md",
                "content": "x",
                "mode": "create"
            })),
        )
        .await;
    assert_eq!(status, 409, "{outside}");
    let _ = rig.handle.request_shutdown();
}

/// The manual compaction route shares the agent's restart invariant: a
/// checkpoint whose recorded digest does not describe its payload is corrupt
/// (or swapped) and must never be re-certified into a new checkpoint.
#[tokio::test]
async fn manual_compaction_ignores_a_checkpoint_with_a_mismatched_digest() {
    let rig = Rig::new().await;
    let handle = rig
        .deps
        .session
        .get_session(faktor_core::id::SessionId::new(rig.session))
        .unwrap()
        .unwrap();
    // The real transcript the fallback MUST preserve. If a refused
    // checkpoint still contributed its `through_seq`, every one of these
    // rows would be skipped while the loader claimed to fall back.
    let mut newest = 0u64;
    for turn in 0..3 {
        let seq = handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::ReadyForNextTurn,
                None,
                None,
            )
            .unwrap();
        newest = seq.raw() as u64;
        let message = handle
            .put_message(seq.raw() as i64, "user", json!({"parts": []}))
            .unwrap();
        handle
            .put_text_part(message, &format!("ROW-{turn:03} durable transcript"))
            .unwrap();
    }
    let payload_turns = vec![faktor_context::RecentTurn {
        role: "user".into(),
        text: "PAYLOAD CONTENT".into(),
    }];
    let digest = faktor_context::history_digest(&[faktor_context::RecentTurn {
        role: "user".into(),
        text: "SOMETHING ELSE".into(),
    }]);
    handle
        .put_context_checkpoint(
            newest,
            &digest,
            &serde_json::to_vec(&payload_turns).unwrap(),
        )
        .unwrap();
    assert!(
        super::decode_checkpoint_with_seq(&handle).0.is_empty(),
        "a mismatched digest must fall back to the durable transcript, never load"
    );
    let (turns, loaded_newest) = super::load_turns(&rig.deps, &handle, 100).unwrap();
    assert!(
        turns
            .iter()
            .any(|turn| turn.text.contains("ROW-000 durable transcript"))
            && turns
                .iter()
                .any(|turn| turn.text.contains("ROW-002 durable transcript")),
        "a refused checkpoint contributes NEITHER turns NOR its high-water mark: {turns:?}"
    );
    assert!(
        !turns
            .iter()
            .any(|turn| turn.text.contains("PAYLOAD CONTENT")),
        "the swapped payload is never re-certified: {turns:?}"
    );
    assert_eq!(
        loaded_newest, newest,
        "the replacement bound comes from the loaded rows, not the refused head"
    );
}

/// CASE INV-COMPACTION-TOOL-EXCERPT: manual compaction renders
/// `evidence://<id>` for huge historical tool output and PERSISTS the
/// replacement context. Reading the durable `excerpt` field (not a legacy
/// `output`/`text` guess) is what makes the tool storm visible at all; the id
/// must resolve through the daemon's durable evidence authority after the
/// request (and a restart).
#[tokio::test]
async fn manual_compaction_evidence_ids_are_durable() {
    let rig = Rig::new().await;
    let handle = rig.handle();
    let marker = "tool-output-marker-42";
    for turn in 0..13 {
        let seq = handle
            .force_append_event(
                faktor_core::event::EventKind::ModelChunkReceived,
                faktor_core::state::AgentState::ReadyForNextTurn,
                None,
                None,
            )
            .unwrap();
        let text = if turn == 11 {
            format!(
                "{marker}\nerror: build failed at src/lib.rs:41\n{}",
                "warning: unused variable x\n".repeat(8_000)
            )
        } else {
            format!("TURN-{turn:03} {}", "h".repeat(2_000))
        };
        let role = if turn == 11 || turn % 2 == 1 {
            "assistant"
        } else {
            "user"
        };
        let role = if turn == 12 { "user" } else { role };
        let message = handle
            .put_message(seq.raw() as i64, role, json!({"parts": []}))
            .unwrap();
        if turn == 11 {
            handle
                .put_tool_result_part(
                    message,
                    "call-1",
                    &faktor_protocol::native::ToolResultBody {
                        excerpt: text,
                        exit_code: Some(1),
                        artifact: None,
                        slice_hint: None,
                    },
                )
                .unwrap();
        } else {
            handle.put_text_part(message, &text).unwrap();
        }
    }
    rig.capture_usage(4_000, 4_000).await;
    let path = format!("/native/session/{}/context/compaction", rig.session);
    let (status, started) = rig
        .send(
            reqwest::Method::POST,
            &path,
            Some(json!({
                "operationId": "op-durable-evidence",
                "targetTokens": 20_000,
                "minReductionRatio": 0.25
            })),
        )
        .await;
    assert_eq!(status, 200, "{started}");
    assert_eq!(started["state"], "applied", "{started}");
    let checkpoint = handle
        .context_checkpoint()
        .unwrap()
        .expect("the manual compaction persisted a replacement");
    let turns: Vec<faktor_context::RecentTurn> =
        serde_json::from_slice(&checkpoint.payload).unwrap();
    let ref_text = turns
        .iter()
        .find(|turn| turn.text.contains("evidence://"))
        .expect("the huge tool output rides as an evidence ref");
    let raw_id = ref_text
        .text
        .split("evidence://")
        .nth(1)
        .unwrap()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap();
    let evidence_id = faktor_evidence::types::EvidenceId(raw_id.parse().unwrap());
    let workspace = handle.row().unwrap().workspace_id;
    let task_id = handle
        .task_id()
        .ok()
        .map(|task| task.raw())
        .filter(|task| *task > 0);
    let ctx =
        faktor_evidence::store::EvidenceAccessContext::new(rig.session, workspace.raw(), task_id);
    let stored = rig
        .deps
        .agent
        .evidence_authority()
        .get_scoped(evidence_id, &ctx)
        .expect("the checkpoint's evidence id resolves durably");
    let backing = stored.backing.expect("backing retained");
    assert!(String::from_utf8_lossy(&backing).contains(marker));
}

/// CASE MC-MANUAL-COMPACTION-RESTART-DIGEST / MC-COMPACTION-TOOL-EXCERPT
/// (mutation witnesses): the manual route must verify the checkpoint digest
/// before loading it and must read the durable tool-result `excerpt` field.
fn manual_compaction_guards_present(source: &str) -> bool {
    source.contains("if checkpoint.digest != actual {") && source.contains(".get(\"excerpt\")")
}

#[test]
fn manual_compaction_restart_and_tool_excerpt_guards_are_pinned() {
    const SOURCE: &str = include_str!("longtask.rs");
    assert!(
        manual_compaction_guards_present(SOURCE),
        "manual compaction lost its restart-digest or tool-excerpt guard"
    );
    let no_digest = SOURCE.replace("if checkpoint.digest != actual {", "if false {");
    assert!(
        !manual_compaction_guards_present(&no_digest),
        "the witness must detect a dropped manual digest comparison"
    );
    let no_excerpt = SOURCE.replace(".get(\"excerpt\")", ".get(\"output\")");
    assert!(
        !manual_compaction_guards_present(&no_excerpt),
        "the witness must detect a regression to the erased tool-result key"
    );
}

/// RAII convergence: normal success, a typed error return and cancellation
/// all remove the live-executor registration, and a second registration for
/// the same key is refused until the first guard drops.
#[test]
fn active_executor_guard_removes_registration_on_success_error_and_cancel() {
    let session = faktor_core::id::SessionId::new(9_001);
    let token = faktor_core::cancellation::CancellationToken::new();
    {
        let guard = super::ActiveCompactionGuard::register(session, "op-success", token.clone())
            .expect("first registration wins");
        assert!(super::operation_is_active(session, "op-success"));
        assert!(
            super::ActiveCompactionGuard::register(session, "op-success", token).is_none(),
            "one durable row must never get two live executors"
        );
        drop(guard);
    }
    assert!(
        !super::operation_is_active(session, "op-success"),
        "normal success must unregister"
    );
    assert!(
        super::ActiveCompactionGuard::register(
            session,
            "op-success",
            faktor_core::cancellation::CancellationToken::new()
        )
        .is_some(),
        "the key is reusable after the first executor ended"
    );

    let session = faktor_core::id::SessionId::new(9_002);
    let result: Result<(), &str> = {
        let _guard = super::ActiveCompactionGuard::register(
            session,
            "op-error",
            faktor_core::cancellation::CancellationToken::new(),
        )
        .expect("error-path registration");
        assert!(super::operation_is_active(session, "op-error"));
        Err("execute failed")
    };
    assert_eq!(result, Err("execute failed"));
    assert!(
        !super::operation_is_active(session, "op-error"),
        "an error return must unregister"
    );

    let session = faktor_core::id::SessionId::new(9_003);
    let token = faktor_core::cancellation::CancellationToken::new();
    let guard = super::ActiveCompactionGuard::register(session, "op-cancel", token.clone())
        .expect("cancel-path registration");
    assert!(super::cancel_active(session, "op-cancel"));
    assert!(token.is_cancelled());
    assert!(
        super::operation_is_active(session, "op-cancel"),
        "cancellation only signals; the executor guard still owns liveness"
    );
    drop(guard);
    assert!(
        !super::operation_is_active(session, "op-cancel"),
        "a cancelled executor must unregister on drop"
    );
}

/// Panic/unwind inside the executor must drop the guard. The durable row is
/// deliberately non-terminal; once the registration is gone, the status read
/// reconciles the residue to `cancelled` instead of reporting a spinner until
/// restart.
#[tokio::test]
async fn panic_during_execute_removes_registration_and_reconciliation_heals() {
    let rig = Rig::new().await;
    let handle = rig.handle();
    let session = handle.id();
    handle
        .begin_compaction_operation("op-unwind", 0, 0.25, None, None)
        .expect("prepared durable row");
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = super::ActiveCompactionGuard::register(
            session,
            "op-unwind",
            faktor_core::cancellation::CancellationToken::new(),
        )
        .expect("executor registration");
        assert!(super::operation_is_active(session, "op-unwind"));
        panic!("executor panicked after registration");
    }));
    assert!(
        unwound.is_err(),
        "the panic must propagate through the test"
    );
    assert!(
        !super::operation_is_active(session, "op-unwind"),
        "an unwound executor must not leave a live registration"
    );
    let path = format!(
        "/native/session/{}/context/compaction/op-unwind",
        rig.session
    );
    let (status, body) = rig.send(reqwest::Method::GET, &path, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["state"], "cancelled",
        "reconciliation must heal the residue after unwind: {body}"
    );
    assert_eq!(body["terminal"], true);
    let _ = rig.handle.request_shutdown();
}

/// CASE MC-COMPACTION-RAII-GUARD (mutation witness): executor registration
/// must be a Drop-owning guard with no manual unregister path left in the
/// start route. Both mutations (deleted Drop, reintroduced manual
/// registration) must make the witness fail.
fn active_compaction_registration_is_raii(source: &str) -> bool {
    source.contains("struct ActiveCompactionGuard")
        && source.contains("impl Drop for ActiveCompactionGuard")
        && source.contains("active.remove(&self.key);")
        && source
            .contains("ActiveCompactionGuard::register(handle.id(), &operation_id, token.clone())")
        && !source.contains("unregister_active(")
}

#[test]
fn active_compaction_registration_is_raii_and_mutation_sensitive() {
    const SOURCE: &str = include_str!("longtask.rs");
    assert!(
        active_compaction_registration_is_raii(SOURCE),
        "executor registration must be RAII (Drop removes the key)"
    );
    let no_drop = SOURCE.replace(
        "impl Drop for ActiveCompactionGuard",
        "impl GuardTeardown for ActiveCompactionGuard",
    );
    assert!(
        !active_compaction_registration_is_raii(&no_drop),
        "the witness must detect a removed Drop implementation"
    );
    let manual = SOURCE.replace(
        "ActiveCompactionGuard::register(handle.id(), &operation_id, token.clone())",
        "legacy_register_active_if_absent(handle.id(), &operation_id, token.clone())",
    );
    assert!(
        !active_compaction_registration_is_raii(&manual),
        "the witness must detect a reintroduced manual registration path"
    );
}
