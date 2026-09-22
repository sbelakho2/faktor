//! Adversarial tests for the additive proof-surfacing routes:
//! `GET /native/tasks/{id}/proof` and `GET /native/tasks/{id}/completion-steps`.
//!
//! Every test tries to break the surface: missing/foreign session scopes,
//! unverified claims presented as VERIFIED, refused completion gates, in-flight
//! landings, failed records, missing steps and typed fail-closed refusals.

use crate::api::tests::test_deps;
use crate::serve;
use faktor_core::completion::{CompletionContract, CompletionStep, CompletionStepOutcome};
use faktor_core::hash::FileHash;
use faktor_core::id::{OpId, TaskId, TaskRevision, VerificationRecordId};
use faktor_core::state::{
    CheckExecution, CriterionBinding, CriterionVerification, FileStateEvidence, TaskState,
    TaskTransition, VerificationStatus,
};
use faktor_session::ledger::{
    ExternalOperationInput, ExternalOperationRow, ExternalOperationState, IntegrationRecordRow,
    IntegrationTxnPhase, IntegrationTxnRow, VerifiedGitArtifact, VerifiedManifestEntry,
};
use faktor_session::task::{Criterion, Task};
use faktor_session::{BudgetAuthority, SessionHandle, SessionManager};
use std::sync::Arc;

fn hex(fill: char) -> String {
    std::iter::repeat_n(fill, 64).collect()
}

fn tm1(fill: char) -> String {
    format!("tm1:{}", hex(fill))
}

fn oid(fill: char) -> String {
    std::iter::repeat_n(fill, 40).collect()
}

struct Fixture {
    _dir: tempfile::TempDir,
    server: crate::ServerHandle,
    handle: SessionHandle,
    manager: Arc<SessionManager>,
    token: String,
    session_id: String,
    task_id: TaskId,
    /// The exact encoded acceptance-criteria entry the record must key on.
    criterion_entry: String,
}

impl Fixture {
    fn base(&self) -> String {
        format!("http://{}", self.server.addr)
    }

    fn get(&self, path: &str, session: Option<&str>) -> reqwest::RequestBuilder {
        let target = match session {
            Some(session) => format!("{}{path}?session={session}", self.base()),
            None => format!("{}{path}", self.base()),
        };
        reqwest::Client::new().get(target).bearer_auth(&self.token)
    }

    /// A second session over the SAME store (a foreign session for scope
    /// tests) with its own task.
    fn second_session(&self) -> (String, SessionHandle) {
        let ws = self
            .manager
            .create_workspace(self._dir.path().join("ws2").to_str().unwrap())
            .unwrap();
        let handle = self
            .manager
            .create_session(ws, "other", "fake", "m")
            .unwrap();
        (handle.id().to_string(), handle)
    }

    /// A completed (verified) task's integration record, transaction and
    /// verified-git artifact — the durable landing the story needs.
    fn seed_landing(&self, run_revision: TaskRevision, record: u64, run_id: &str, snapshot: &str) {
        self.handle
            .ledger_integration_record_set(&IntegrationRecordRow {
                run_id: run_id.into(),
                task_id: self.task_id.raw(),
                base_revision: None,
                base_snapshot: None,
                run_base_snapshot: Some(tm1('a')),
                candidate_snapshot: Some(snapshot.to_string()),
                landed_snapshot: Some(snapshot.to_string()),
                proof_basis_digest: Some("basis-1".into()),
                integration_txn_id: Some("txn-1".into()),
                final_root: "/tmp/root".into(),
                final_snapshot_hash: snapshot.to_string(),
                integrated_files: vec!["src/lib.rs".into()],
                integrated_file_count: 1,
                integrated_files_digest: hex('f'),
                conflicts: vec![],
                conflict_count: 0,
                sources: vec![],
                source_count: 1,
                sources_digest: hex('3'),
                at_ms: 5,
            })
            .unwrap();
        self.handle
            .ledger_integration_txn_set(&IntegrationTxnRow {
                run_id: run_id.into(),
                task_id: self.task_id.raw(),
                owner_root: "/tmp/owner".into(),
                candidate_root: "/tmp/candidate".into(),
                run_base_snapshot: tm1('a'),
                verified_candidate_snapshot: snapshot.to_string(),
                sources_digest: String::new(),
                phase: IntegrationTxnPhase::Landed,
                paths: vec![],
                path_count: 0,
                applied_count: 0,
                conflicts: vec![],
                at_ms: 6,
            })
            .unwrap();
        self.handle
            .ledger_verified_git_artifact_set(&VerifiedGitArtifact {
                task_id: self.task_id.raw(),
                revision: run_revision.raw(),
                verification_record: record,
                verified_root_digest: snapshot.to_string(),
                verified_manifest: vec![VerifiedManifestEntry {
                    path: "src/lib.rs".into(),
                    state: faktor_fs::entry_state::EntryState::regular(
                        faktor_fs::tree_manifest::CanonicalMode::RegularFile,
                        FileHash::from_hex(&hex('e')).unwrap(),
                    )
                    .unwrap(),
                }],
                git_tree_oid: Some(oid('1')),
                commit_oid: Some(oid('2')),
                local_ref: Some("refs/heads/main".into()),
                remote_ref: Some(format!("origin:refs/heads/main@{}", oid('2'))),
                updated_ms: 7,
            })
            .unwrap();
    }
}

/// A live daemon + one session with a dedicated (empty) workspace root. The
/// seeded task is created in `Running` with one bound acceptance criterion.
async fn fixture(task_id: u64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let ws_root = dir.path().join("ws");
    std::fs::create_dir_all(&ws_root).unwrap();
    let deps = test_deps(dir.path());
    let manager = deps.session.clone();
    let budgets = deps.budgets.clone();
    let token = deps.auth_token.as_str().to_string();
    let server = serve(deps, 0).await.unwrap();
    let ws = manager.create_workspace(ws_root.to_str().unwrap()).unwrap();
    let handle = manager
        .create_session(ws, "proof-test", "fake", "m")
        .unwrap();
    let session_id = handle.id().to_string();
    let task_id = TaskId::new(task_id);
    let criterion =
        Criterion::user("the acceptance criterion").with_binding(CriterionBinding::RequiredCheck {
            check_id: "check-c1".into(),
            command_digest: hex('d'),
        });
    let criterion_entry = criterion.encoded_entry();
    let task = Task {
        task_id,
        session_id: handle.id(),
        goal: "prove the story".into(),
        acceptance_criteria: vec![criterion_entry.clone()],
        state: TaskState::Running,
        created_ms: 1,
        updated_ms: 1,
        ..Task::default()
    };
    handle.create_task(task).unwrap();
    // The spend fold is seeded while the task still permits provider work:
    // a hard cap plus one settled provider-reported spend (the completion
    // accounting folds it before the transition).
    budgets
        .set_task_max_cost(handle.id(), task_id, Some(1_000_000))
        .unwrap();
    let reservation = budgets
        .reserve(handle.id(), task_id, OpId::new(901), 500, None)
        .await
        .unwrap();
    budgets
        .settle_usage(handle.id(), reservation, 0, 0, 0, 0, Some(12), None)
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        server,
        handle,
        manager,
        token,
        session_id,
        task_id,
        criterion_entry,
    }
}

/// Drive Running -> NeedsVerification -> Verifying and return the revision.
fn drive_to_verifying(handle: &SessionHandle, task_id: TaskId) -> TaskRevision {
    let rev = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, rev, TaskTransition::RequestVerification, None)
        .unwrap();
    let rev = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, rev, TaskTransition::StartVerification, None)
        .unwrap();
    handle.task_revision(task_id).unwrap()
}

/// Create the passing manifest-bound verification record that certifies the
/// current revision. Returns the record id.
fn passing_record(handle: &SessionHandle, task_id: TaskId, tree: &str, criterion_key: &str) -> u64 {
    let checks = vec![CheckExecution {
        check: "check-c1".into(),
        program: "cargo".into(),
        args: vec!["test".into()],
        category: "test".into(),
        required: true,
        status: VerificationStatus::Passed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(0),
        summary: Some("ok".into()),
    }];
    let criteria = vec![CriterionVerification {
        criterion_key: criterion_key.to_string(),
        passed: true,
        evidence: Some("check:check-c1".into()),
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "check-c1".into(),
            command_digest: hex('d'),
        }),
    }];
    let changed = vec![FileStateEvidence {
        path: "src/lib.rs".into(),
        digest_hex: hex('e'),
        size: 3,
    }];
    handle
        .create_verification_record(
            task_id,
            Some(tree.to_string()),
            criteria,
            checks,
            changed,
            vec![],
            Some(serde_json::json!({"id": "reviewer-1", "independent": true})),
            VerificationStatus::Passed,
            1,
        )
        .unwrap()
        .raw()
}

/// A fully verified, manifest-bound task: contract (push+pr) on the run
/// revision, both steps succeeded, the landing record, the passing record
/// bound to the live workspace root digest, then completion. Returns
/// `(run_revision, record_id, snapshot)`.
fn verified_task(f: &Fixture) -> (TaskRevision, u64, String) {
    let run_revision = drive_to_verifying(&f.handle, f.task_id);
    f.handle
        .set_completion_contract(
            f.task_id,
            run_revision,
            CompletionContract {
                include_commit: false,
                include_push: true,
                include_pr: true,
            },
        )
        .unwrap();
    for step in [CompletionStep::Push, CompletionStep::Pr] {
        f.handle
            .set_completion_step_status(f.task_id, step, CompletionStepOutcome::Succeeded, "done")
            .unwrap();
    }
    // The live root digest the manifest-bound completion compares against.
    let snapshot = f
        .handle
        .current_root_snapshot_digest()
        .unwrap()
        .expect("the workspace root is resolvable");
    f.seed_landing(run_revision, 1, "run-1", &snapshot);
    let record = passing_record(&f.handle, f.task_id, &snapshot, &f.criterion_entry);
    f.handle
        .complete_verified_task(f.task_id, run_revision, VerificationRecordId::new(record))
        .unwrap();
    (run_revision, record, snapshot)
}

#[tokio::test]
async fn proof_route_is_session_scoped_and_refuses_foreign_or_unknown_tasks() {
    let f = fixture(42).await;
    // No scope: loud typed 400 (never an unscoped peek).
    let resp = f.get("/native/tasks/42/proof", None).send().await.unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "malformed");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("session"));

    // Unknown session: typed 404.
    let resp = f
        .get("/native/tasks/42/proof", Some("9999"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);

    // A second session (same store) cannot read task 42 of the first.
    let (other_session, _other_handle) = f.second_session();
    assert_ne!(f.session_id, other_session, "two distinct sessions");
    let resp = f
        .get("/native/tasks/42/proof", Some(&other_session))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "a task is scoped to its owning session");
    assert!(
        resp.text().await.unwrap().contains("unknown task"),
        "the 404 names the unknown task"
    );

    // Non-numeric and zero ids are malformed, never a panic.
    let resp = f
        .get("/native/tasks/zero/proof", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = f
        .get("/native/tasks/0/proof", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // The completion-steps route shares the exact scope discipline.
    let resp = f
        .get("/native/tasks/42/completion-steps", None)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let resp = f
        .get("/native/tasks/42/completion-steps", Some(&other_session))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn verified_story_is_served_in_one_strict_payload() {
    let f = fixture(42).await;
    let (run_revision, record, snapshot) = verified_task(&f);

    // The published pull request of the completion contract revision.
    let input = ExternalOperationInput {
        organization: "acme".into(),
        repository: "widgets".into(),
        head: oid('2'),
        base: "main".into(),
        marker: format!("faktor:task:{}:rev:{}", f.task_id.raw(), run_revision.raw()),
    };
    let pr_key = format!(
        "task:{}:rev:{}:github:pull_request",
        f.task_id.raw(),
        run_revision.raw()
    );
    let completed = ExternalOperationRow {
        id: ExternalOperationRow::content_id(&pr_key, "github", "pull_request", &input),
        operation_key: pr_key,
        provider: "github".into(),
        kind: "pull_request".into(),
        input,
        state: ExternalOperationState::Completed,
        remote_object_id: Some("pr-42".into()),
        remote_object_version: Some(format!("{}@1700000000", oid('2'))),
        started_at: 8,
        reconciled_at: Some(9),
    };
    f.handle.ledger_external_operation_set(&completed).unwrap();
    let _ = record;

    let resp = f
        .get("/native/tasks/42/proof", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body["schema"], "faktor-task-proof/v1");
    assert_eq!(body["sessionId"], f.session_id);
    assert_eq!(body["taskId"], "42");
    assert_eq!(body["proofState"], "verified");

    // Criteria + checks + review.
    assert_eq!(body["criteria"]["total"], 1);
    assert_eq!(body["criteria"]["passed"], 1);
    assert_eq!(body["criteria"]["failed"], 0);
    assert_eq!(body["criteria"]["certifiesCompletion"], true);
    assert_eq!(body["criteria"]["items"][0]["verdict"], "pass");
    assert_eq!(
        body["criteria"]["items"][0]["bindingKind"],
        "required_check"
    );
    assert_eq!(body["criteria"]["items"][0]["evidence"], "check:check-c1");
    assert_eq!(body["checks"]["requiredTotal"], 1);
    assert_eq!(body["checks"]["requiredPassed"], 1);
    assert_eq!(body["checks"]["requiredAllPassed"], true);
    assert_eq!(body["review"]["status"], "passed");
    assert_eq!(body["review"]["reviewer"]["id"], "reviewer-1");
    assert_eq!(body["review"]["independentVerdict"], "none");

    // Trees: verified == landed, run base served.
    assert_eq!(body["trees"]["verified"], snapshot);
    assert_eq!(body["trees"]["landed"], snapshot);
    assert_eq!(body["trees"]["landedEqualsVerified"], true);
    assert_eq!(body["trees"]["runBase"], tm1('a'));

    // Publication: commit + remote head + PR head.
    assert_eq!(body["publication"]["commitOid"], oid('2'));
    assert_eq!(body["publication"]["remoteHeadOid"], oid('2'));
    assert_eq!(body["publication"]["localRef"], "refs/heads/main");
    assert_eq!(body["publication"]["gitTreeOid"], oid('1'));
    assert_eq!(body["publication"]["pullRequest"]["id"], "pr-42");
    assert_eq!(body["publication"]["pullRequest"]["headOid"], oid('2'));

    // Cost fold: money fields are decimal strings on the wire.
    assert_eq!(body["cost"]["status"], "known");
    assert_eq!(body["cost"]["maxCostMicro"], "1000000");
    assert_eq!(body["cost"]["spentCostMicro"], "12");
    assert_eq!(body["cost"]["settledCount"], 1);
    assert_eq!(body["cost"]["openReservations"], 0);

    // Completion steps + gate + integration txn.
    assert_eq!(body["completion"]["contract"]["includePush"], true);
    assert_eq!(body["completion"]["contract"]["includeCommit"], false);
    assert_eq!(body["completion"]["gate"]["status"], "satisfied");
    let steps = body["completion"]["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["step"], "push");
    assert_eq!(steps[0]["status"], "succeeded");
    assert_eq!(steps[1]["step"], "pr");
    assert_eq!(steps[1]["present"], true);
    assert_eq!(body["integration"]["present"], true);
    assert_eq!(body["integration"]["inFlight"], false);
    assert_eq!(body["integration"]["finalSnapshotHash"], snapshot);
    assert_eq!(body["integration"]["txn"]["phase"], "landed");
    assert!(
        body["integration"]["txn"]["txnId"]
            .as_str()
            .unwrap()
            .starts_with("blake3:"),
        "the transaction id is the deterministic content id: {body}"
    );
    assert!(
        body["unavailable"].as_array().unwrap().is_empty(),
        "a verified story carries no unavailable components: {body}"
    );
}

#[tokio::test]
async fn completion_steps_route_serves_statuses_and_missing_rows() {
    let f = fixture(42).await;
    // No contract yet: the gate is `no_contract` and nothing is claimed.
    let resp = f
        .get("/native/tasks/42/completion-steps", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["schema"], "faktor-task-completion-steps/v1");
    assert_eq!(body["contract"], serde_json::Value::Null);
    assert_eq!(body["gate"]["status"], "no_contract");
    assert!(body["steps"].as_array().unwrap().is_empty());

    // A contract with one succeeded and one missing step.
    let rev = drive_to_verifying(&f.handle, f.task_id);
    f.handle
        .set_completion_contract(
            f.task_id,
            rev,
            CompletionContract {
                include_commit: false,
                include_push: true,
                include_pr: true,
            },
        )
        .unwrap();
    f.handle
        .set_completion_step_status(
            f.task_id,
            CompletionStep::Push,
            CompletionStepOutcome::Succeeded,
            "pushed to origin",
        )
        .unwrap();
    let resp = f
        .get("/native/tasks/42/completion-steps", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["contract"]["includePush"], true);
    assert_eq!(body["contract"]["revision"], rev.to_string());
    let steps = body["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0]["step"], "push");
    assert_eq!(steps[0]["status"], "succeeded");
    assert_eq!(steps[0]["detail"], "pushed to origin");
    assert_eq!(steps[1]["step"], "pr");
    assert_eq!(steps[1]["status"], "missing");
    assert_eq!(steps[1]["present"], false);
    assert_eq!(steps[1]["detail"], serde_json::Value::Null);
    assert_eq!(body["gate"]["status"], "refused");
    assert!(
        body["gate"]["reason"].as_str().unwrap().contains("Pr"),
        "the refusal names the missing step: {body}"
    );
    // The history carries only durable rows (the missing step has none).
    assert_eq!(body["records"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn verified_claim_with_a_refused_gate_is_unavailable_never_fake_verified() {
    let f = fixture(42).await;
    let (_run_revision, _record, _snapshot) = verified_task(&f);
    let final_revision = f.handle.task_revision(f.task_id).unwrap();
    // A NEW contract lands after completion (post-completion drift): its
    // requested step has no durable row, so the proof chain no longer backs
    // the VerifiedComplete claim.
    f.handle
        .set_completion_contract(
            f.task_id,
            final_revision,
            CompletionContract {
                include_commit: true,
                include_push: false,
                include_pr: false,
            },
        )
        .unwrap();
    let resp = f
        .get("/native/tasks/42/proof", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["task"]["state"], "verified_complete");
    assert_eq!(body["proofState"], "unavailable");
    assert!(
        body["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["component"] == "completion_gate" && u["kind"] == "unavailable"),
        "the refused gate is named explicitly: {body}"
    );
    assert_ne!(body["proofState"], "verified");
}

#[tokio::test]
async fn in_flight_landing_is_unavailable_never_verified() {
    let f = fixture(42).await;
    let (_run_revision, _record, _snapshot) = verified_task(&f);
    // A NEWER in-flight integration record (record-first): the final
    // snapshot hash is EMPTY and no landed snapshot exists.
    f.handle
        .ledger_integration_record_set(&IntegrationRecordRow {
            run_id: "run-inflight".into(),
            task_id: f.task_id.raw(),
            base_revision: None,
            base_snapshot: None,
            run_base_snapshot: Some(tm1('a')),
            candidate_snapshot: Some(tm1('c')),
            landed_snapshot: None,
            proof_basis_digest: None,
            integration_txn_id: None,
            final_root: "/tmp/inflight".into(),
            final_snapshot_hash: String::new(),
            integrated_files: vec![],
            integrated_file_count: 0,
            integrated_files_digest: String::new(),
            conflicts: vec![],
            conflict_count: 0,
            sources: vec![],
            source_count: 1,
            sources_digest: hex('3'),
            at_ms: 5,
        })
        .unwrap();
    let resp = f
        .get("/native/tasks/42/proof", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["proofState"], "unavailable");
    assert_eq!(body["integration"]["inFlight"], true);
    assert_eq!(body["trees"]["landed"], serde_json::Value::Null);
    assert!(
        body["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["component"] == "integration_record" && u["kind"] == "unavailable"),
        "the in-flight landing is named explicitly: {body}"
    );
}

#[tokio::test]
async fn a_failed_record_is_unverified_and_never_renders_as_verified() {
    let f = fixture(42).await;
    let _rev = drive_to_verifying(&f.handle, f.task_id);
    let criteria = vec![CriterionVerification {
        criterion_key: f.criterion_entry.clone(),
        passed: false,
        evidence: None,
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "check-c1".into(),
            command_digest: hex('d'),
        }),
    }];
    let checks = vec![CheckExecution {
        check: "check-c1".into(),
        program: "cargo".into(),
        args: vec![],
        category: "test".into(),
        required: true,
        status: VerificationStatus::Failed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(1),
        summary: Some("boom".into()),
    }];
    f.handle
        .create_verification_record(
            f.task_id,
            None,
            criteria,
            checks,
            vec![],
            vec![],
            None,
            VerificationStatus::Failed,
            1,
        )
        .unwrap();
    let resp = f
        .get("/native/tasks/42/proof", Some(&f.session_id))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["proofState"], "unverified");
    assert_eq!(body["criteria"]["failed"], 1);
    assert_eq!(body["checks"]["requiredFailed"], 1);
    assert_eq!(body["criteria"]["certifiesCompletion"], false);
    assert_eq!(body["task"]["state"], "verifying");
    // The newest record is projected, but it is NOT certifying.
    assert_eq!(body["review"]["status"], "failed");
    assert_ne!(body["proofState"], "verified");
}

#[tokio::test]
async fn corrupt_present_components_fail_closed_with_a_typed_500() {
    use axum::response::IntoResponse;
    let resp = crate::native::proof_corrupt("integration record", "bad json").into_response();
    assert_eq!(resp.status(), 500);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "corrupt_durable_state");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("integration record"));

    let resp = crate::native::proof_store_failure("run base record", "disk gone").into_response();
    assert_eq!(resp.status(), 500);
    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "internal");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("run base record"));
}
