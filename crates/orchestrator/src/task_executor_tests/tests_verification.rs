#![allow(clippy::await_holding_lock)]
//! TaskExecutor tests: proof/verification composition, placement, remote and queue boundaries (mechanically split from `task_executor_tests`).

use super::tests_control::*;
use super::tests_core::*;
use super::*;

// ------------------------------------------------- composition unit coverage

pub(crate) fn verdict_check(
    id: &str,
    required: bool,
    status: VerificationStatus,
) -> CheckExecution {
    CheckExecution {
        check: id.into(),
        program: "cargo".into(),
        args: vec!["check".into()],
        category: "compile".into(),
        required,
        status,
        started_ms: 1,
        finished_ms: Some(2),
        exit: None,
        summary: None,
    }
}

pub(crate) fn verdict_criterion(
    criterion_key: &str,
    passed: bool,
    evidence: Option<&str>,
) -> CriterionVerification {
    CriterionVerification {
        criterion_key: criterion_key.into(),
        passed,
        evidence: evidence.map(str::to_string),
        binding: None,
    }
}

pub(crate) fn required_criterion_entry(text: &str) -> String {
    faktor_session::task::Criterion::derived(
        text,
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .encode()
}

pub(crate) fn advisory_criterion_entry(text: &str) -> String {
    faktor_session::task::Criterion::derived(
        text,
        CriterionOrigin::User,
        CriterionRequirement::Preferred,
        None,
    )
    .encode()
}

/// P0: the composer passes ONLY when every required check AND every required
/// criterion passes; advisory verdicts never block; no unknown state ever
/// surfaces as `Pending` (unavailable/missing evidence becomes the explicit
/// `Unavailable`).
#[test]
pub(crate) fn compose_root_verdict_is_required_only_and_never_pending() {
    let required_key = required_criterion_entry("the build is green");
    let advisory_key = advisory_criterion_entry("the docs read nicely");
    let green = vec![verdict_check(
        "rust_check",
        true,
        VerificationStatus::Passed,
    )];
    let all_pass = vec![verdict_criterion(
        &required_key,
        true,
        Some("check:rust_check"),
    )];
    // Control: every required check and criterion passes.
    assert_eq!(
        compose_root_verification_status(&green, &all_pass),
        VerificationStatus::Passed
    );
    // Advisory failure (and advisory unavailability) never blocks.
    assert_eq!(
        compose_root_verification_status(
            &green,
            &[
                verdict_criterion(&required_key, true, Some("check:rust_check")),
                verdict_criterion(&advisory_key, false, Some("style note")),
                verdict_criterion(&advisory_key, false, None),
            ],
        ),
        VerificationStatus::Passed
    );
    // A required criterion with missing evidence is Unavailable, never
    // Pending and never a pass.
    assert_eq!(
        compose_root_verification_status(&green, &[verdict_criterion(&required_key, false, None)]),
        VerificationStatus::Unavailable
    );
    // The explicit honest-unknown binding is Unavailable even with prose
    // "evidence".
    let explicit_unknown = faktor_session::task::Criterion::derived(
        "honest unknown",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::Unavailable {
        reason: "no mechanism".into(),
    })
    .encode();
    let mut unknown_verdict = verdict_criterion(&explicit_unknown, false, Some("no mechanism"));
    unknown_verdict.binding = Some(CriterionBinding::Unavailable {
        reason: "no mechanism".into(),
    });
    assert_eq!(
        compose_root_verification_status(&green, &[unknown_verdict]),
        VerificationStatus::Unavailable
    );
    // A failed required criterion with evidence is Failed.
    assert_eq!(
        compose_root_verification_status(
            &green,
            &[verdict_criterion(&required_key, false, Some("file:x.rs"))],
        ),
        VerificationStatus::Failed
    );
    // A required-check binding that resolves to nothing is missing evidence.
    let mut unresolved = verdict_criterion(&required_key, false, Some("check:rust_check"));
    unresolved.binding = Some(CriterionBinding::RequiredCheck {
        check_id: "rust_check".into(),
        command_digest: "digest-that-never-ran".into(),
    });
    assert_eq!(
        compose_root_verification_status(&green, &[unresolved]),
        VerificationStatus::Unavailable
    );
    // Legacy plain-text criteria stay Required (`Criterion::legacy`).
    assert_eq!(
        compose_root_verification_status(
            &green,
            &[verdict_criterion("plain legacy prose", false, Some("note"))],
        ),
        VerificationStatus::Failed
    );
    // Check side: failed => Failed; unavailable/pending => Unavailable;
    // optional checks never block.
    assert_eq!(
        compose_root_verification_status(
            &[verdict_check(
                "rust_check",
                true,
                VerificationStatus::Failed
            )],
            &[],
        ),
        VerificationStatus::Failed
    );
    for status in [
        VerificationStatus::Unavailable,
        VerificationStatus::Pending,
        VerificationStatus::Running,
    ] {
        assert_eq!(
            compose_root_verification_status(&[verdict_check("rust_check", true, status)], &[]),
            VerificationStatus::Unavailable,
            "{status:?} must never compose Pending"
        );
    }
    assert_eq!(
        compose_root_verification_status(
            &[
                verdict_check("rust_check", true, VerificationStatus::Passed),
                verdict_check("advisory_bench", false, VerificationStatus::Failed),
            ],
            &[verdict_criterion(&required_key, true, Some("ok"))],
        ),
        VerificationStatus::Passed
    );
    // Failure dominates unavailability when both required sides are bad.
    assert_eq!(
        compose_root_verification_status(
            &[verdict_check(
                "rust_check",
                true,
                VerificationStatus::Failed
            )],
            &[verdict_criterion(&required_key, false, None)],
        ),
        VerificationStatus::Failed
    );
}

/// The no-op rule is STRICTER: on an empty aggregate change set an advisory
/// failure also blocks (there is no check evidence to lean on).
#[test]
pub(crate) fn compose_no_op_verdict_requires_every_criterion() {
    let required_key = required_criterion_entry("the build is green");
    let advisory_key = advisory_criterion_entry("the docs read nicely");
    let all_pass = vec![
        verdict_criterion(&required_key, true, Some("check:rust_check")),
        verdict_criterion(&advisory_key, true, Some("prose reviewed")),
    ];
    assert_eq!(
        compose_no_op_root_verification_status(&[], &all_pass),
        VerificationStatus::Passed
    );
    assert_eq!(
        compose_no_op_root_verification_status(
            &[],
            &[verdict_criterion(&advisory_key, false, Some("style note"))],
        ),
        VerificationStatus::Failed,
        "an advisory failure cannot pass the no-op rule"
    );
    assert_eq!(
        compose_no_op_root_verification_status(
            &[],
            &[verdict_criterion(&advisory_key, false, None)],
        ),
        VerificationStatus::Unavailable
    );
    // The required-only composer, by contrast, ignores the same advisory
    // failure.
    assert_eq!(
        compose_root_verification_status(
            &[],
            &[verdict_criterion(&advisory_key, false, Some("style note"))],
        ),
        VerificationStatus::Passed
    );
}

/// The new explicit variant is a first-class durable value: a record written
/// with `Unavailable` survives a real store reopen byte-for-byte, and the
/// root verification fact carries its own durable tag.
#[tokio::test]
pub(crate) async fn unavailable_verdict_round_trips_through_a_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    let parent = env.parent;
    let h = env.manager.get_session(parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: parent,
        goal: "round-trip".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let record_id = h
        .create_verification_record(
            task_id,
            Some("a".repeat(64)),
            vec![CriterionVerification {
                criterion_key: "criterion".into(),
                passed: false,
                evidence: Some("no objective mechanism ran".into()),
                binding: Some(CriterionBinding::Unavailable {
                    reason: "no objective mechanism ran".into(),
                }),
            }],
            vec![verdict_check(
                "rust_check",
                true,
                VerificationStatus::Unavailable,
            )],
            vec![],
            vec![],
            None,
            VerificationStatus::Unavailable,
            now,
        )
        .unwrap();
    crate::runtime::task_executor::persist_root_verification_fact(&h, "unavailable", &[], &[])
        .unwrap();
    drop(h);
    drop(env);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(parent).unwrap().unwrap();
    let record = h2
        .get_verification_record(record_id)
        .unwrap()
        .expect("the Unavailable record survives the reopen");
    assert_eq!(record.status, VerificationStatus::Unavailable);
    assert_eq!(
        record.checks[0].status,
        VerificationStatus::Unavailable,
        "check rows round-trip the variant too"
    );
    assert_eq!(
        record.criteria[0].binding,
        Some(CriterionBinding::Unavailable {
            reason: "no objective mechanism ran".into()
        })
    );
    let fact = h2
        .memory_facts()
        .unwrap()
        .into_iter()
        .find(|(kind, key, _)| kind == "verification" && key == "last")
        .expect("the root verification fact");
    let fact: serde_json::Value = serde_json::from_str(&fact.2).unwrap();
    assert_eq!(fact["status"], "unavailable");
}

pub(crate) fn unit_cs(
    child: &str,
    files: Vec<crate::runtime::merge::ChangeEntry>,
) -> crate::runtime::merge::ChangeSet {
    crate::runtime::merge::ChangeSet {
        child_id: child.to_string(),
        base_id: format!("base-{child}"),
        run_base_snapshot: Some("a".repeat(64)),
        child_start_snapshot: None,
        final_child_snapshot: None,
        files,
        created_ms: 1,
    }
}

pub(crate) fn unit_entry(
    path: &str,
    child_hash: Option<FileHash>,
    base_hash: Option<FileHash>,
) -> crate::runtime::merge::ChangeEntry {
    use faktor_fs::entry_state::EntryState;
    let state = |hash: FileHash| {
        EntryState::regular(faktor_fs::tree_manifest::CanonicalMode::RegularFile, hash)
            .expect("regular state")
    };
    crate::runtime::merge::ChangeEntry {
        path: std::path::PathBuf::from(path),
        child_hash,
        base_hash,
        child: child_hash.map(state),
        base: base_hash.map(state),
    }
}

/// Point 4: three children converging on the SAME resulting hash (same
/// file, same content) are applied once with provenance retained; every
/// input permutation resolves identically (lexical order is not the
/// resolution mechanism).
#[test]
pub(crate) fn convergent_multi_child_same_file_is_deterministic() {
    let base = FileHash::from([7u8; 32]);
    let result = FileHash::from([9u8; 32]);
    let make = |order: &[usize]| {
        let mut children: Vec<crate::runtime::merge::ChangeSet> = Vec::new();
        for i in order {
            children.push(unit_cs(
                &format!("child-{i}"),
                vec![
                    unit_entry("same.rs", Some(result), Some(base)),
                    unit_entry(
                        &format!("only-{i}.rs"),
                        Some(FileHash::from([*i as u8; 32])),
                        None,
                    ),
                ],
            ));
        }
        crate::runtime::merge::compose_child_changes(&children).unwrap()
    };
    let reference = make(&[0, 1, 2]);
    let same = reference
        .iter()
        .find(|c| c.path.ends_with("same.rs"))
        .unwrap();
    assert_eq!(same.sources, vec!["child-0", "child-1", "child-2"]);
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        assert_eq!(make(&order), reference, "order {order:?} drifted");
    }
    let paths: Vec<_> = reference.iter().map(|c| c.path.clone()).collect();
    assert_eq!(paths, {
        let mut sorted = paths.clone();
        sorted.sort();
        sorted
    });
}

/// Point 4: two children producing DIFFERENT hashes for the same path are a
/// typed conflict (no order-based resolution exists).
#[test]
pub(crate) fn divergent_multi_child_same_file_conflicts() {
    let base = FileHash::from([7u8; 32]);
    let children = vec![
        unit_cs(
            "child-0",
            vec![unit_entry(
                "same.rs",
                Some(FileHash::from([1u8; 32])),
                Some(base),
            )],
        ),
        unit_cs(
            "child-1",
            vec![unit_entry(
                "same.rs",
                Some(FileHash::from([2u8; 32])),
                Some(base),
            )],
        ),
    ];
    let err = crate::runtime::merge::compose_child_changes(&children).unwrap_err();
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
}

/// Point 4: delete-vs-modify on one path is a typed conflict.
#[test]
pub(crate) fn delete_vs_modify_conflicts() {
    let base = FileHash::from([7u8; 32]);
    let children = vec![
        unit_cs("child-0", vec![unit_entry("same.rs", None, Some(base))]),
        unit_cs(
            "child-1",
            vec![unit_entry(
                "same.rs",
                Some(FileHash::from([3u8; 32])),
                Some(base),
            )],
        ),
    ];
    let err = crate::runtime::merge::compose_child_changes(&children).unwrap_err();
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
}

/// Point 8: the tournament WINNER lands through the SAME pipeline — the
/// decided winner is integrated, verified over the candidate, committed
/// into the owner and certified; the loser's work never reaches the owner.
#[tokio::test]
pub(crate) async fn tournament_winner_lands_through_the_one_pipeline() {
    use crate::runtime::task_executor::TournamentStartRequest;
    use crate::tournament::{CandidateState, ReviewRank, ReviewVerdict};
    use faktor_core::id::VerificationRecordId;

    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..6)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_real_tool_env_full(
        dir.path(),
        scripts,
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let receipt = env
        .executor
        .start_tournament_with(
            env.parent,
            TournamentStartRequest {
                goal: "pick a winner".to_string(),
                criteria: vec![typed_land_criterion()],
                n: 2,
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("tournament start");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    for row in &rows {
        let root = child_root(&env.manager, row);
        std::fs::write(
            root.join("winner.rs"),
            format!(
                "pub fn winner() -> u64 {{\n    let seed: u64 = {0};\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}}\n",
                row.child_id.trim_start_matches("child-").parse::<u64>().unwrap_or(1) + 1
            ),
        )
        .unwrap();
    }
    let mut loser = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-0",
            "verification failed",
        )
        .unwrap();
    loser.verification = Some(VerificationRecordId::new(1));
    loser.verification_pass = Some(false);
    loser.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, loser)
        .unwrap();
    let mut winner = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-1",
            "verified complete",
        )
        .unwrap();
    winner.verification = Some(VerificationRecordId::new(2));
    winner.verification_pass = Some(true);
    winner.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, winner)
        .unwrap();
    let decision = env
        .executor
        .decide_tournament(env.parent, &receipt.tournament_id)
        .expect("deterministic decision");
    assert_eq!(decision.winner.child_id, "child-1");
    assert_eq!(decision.winner.state, CandidateState::Done);
    // The SAME settlement pipeline lands the winner through the candidate.
    let outcome = settle_orchestrated(&env, &receipt.run_id)
        .await
        .expect("winner settlement");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(env.owner_root.join("winner.rs")).unwrap(),
        "pub fn winner() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"
    );
    let integrated = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .ledger_integration_record_for_task(
            env.manager
                .get_session(env.parent)
                .unwrap()
                .unwrap()
                .task_id()
                .unwrap()
                .raw(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(integrated.source_count, 1, "{integrated:?}");
    assert_eq!(integrated.sources[0].child_id, "child-1");
    assert!(!integrated.final_snapshot_hash.is_empty());
    // Hardening: the explicit identity fields are populated from the run's
    // real base/candidate/landed snapshots — and the overloaded
    // `base_revision` (formerly derived from `sources.first()`) stays
    // unpopulated.
    assert!(
        integrated.base_revision.is_none(),
        "sources.first() derivation is gone: {integrated:?}"
    );
    assert_eq!(
        integrated.base_snapshot, integrated.run_base_snapshot,
        "deprecated alias and explicit run base agree"
    );
    assert!(integrated.run_base_snapshot.is_some());
    assert!(integrated.candidate_snapshot.is_some());
    assert_eq!(
        integrated.landed_snapshot.as_deref(),
        Some(integrated.final_snapshot_hash.as_str()),
        "landed identity equals the finalized snapshot"
    );
    let (txn, txn_id) = {
        let h = env.manager.get_session(env.parent).unwrap().unwrap();
        let txn = h
            .ledger_integration_txn_for_run(&receipt.run_id)
            .unwrap()
            .unwrap();
        (txn.clone(), txn.txn_id())
    };
    assert_eq!(
        integrated.integration_txn_id.as_deref(),
        Some(txn_id.as_str()),
        "the record names the exact landing transaction: {txn:?}"
    );
    let basis_digest = {
        let h = env.manager.get_session(env.parent).unwrap().unwrap();
        let task_id = h.task_id().unwrap();
        h.list_verification_records(task_id)
            .unwrap()
            .into_iter()
            .filter(|r| r.status == VerificationStatus::Passed)
            .max_by_key(|r| r.record_id)
            .and_then(|r| r.environment_fingerprint.and_then(|f| f.proof_basis_digest))
            .expect("the passing root record carries its proof basis")
    };
    assert_eq!(
        integrated.proof_basis_digest.as_deref(),
        Some(basis_digest.as_str()),
        "the integration record binds the exact verification basis"
    );
}

/// P1 binary attachments: a stored `AttachmentId` set admits durably on the
/// task row AND the run's linkage row (SEPARATE from `files`), survives a
/// store reopen, and an unknown digest is refused BEFORE any run/task row —
/// no partial durable admission. Image ids are structurally valid here;
/// model-aware delivery (vision/mime/size) is validated at the server DTO
/// and resolved into media parts at agent request construction.
#[tokio::test]
pub(crate) async fn binary_attachments_admit_durably_and_unknown_digests_leave_no_run() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![vec![ScriptedResponse::End]]);
    let parent = env.parent;
    let handle = env.manager.get_session(parent).unwrap().unwrap();
    let stored = handle
        .put_attachment("application/pdf", Some("spec.pdf"), b"%PDF-1.4 spec")
        .unwrap();
    // An unknown digest is refused by the EXECUTOR before any run/task row:
    // no partial durable admission.
    let unknown = faktor_core::attachment::AttachmentId {
        digest: faktor_core::hash::FileHash::from([9; 32]),
        ..stored.clone()
    };
    let mut bad = request("unknown", vec![wi("a1", WorkKind::Analysis, &[])], &env);
    bad.attachments = vec![unknown];
    let err = env
        .executor
        .start_task(parent, bad)
        .expect_err("unknown digest must be refused");
    assert!(matches!(err, ExecError::NotFound(_)), "{err:?}");
    assert!(handle
        .get_task(handle.task_id().unwrap())
        .unwrap()
        .is_none());
    assert!(handle.memory_facts().unwrap().is_empty());

    let mut req = request(
        "attached goal",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.attachments = vec![stored.clone()];
    let receipt = env
        .executor
        .start_task(parent, req)
        .expect("attachment admission must be accepted");
    // Durable Task row carries the typed set, separate from workspace paths.
    let task = handle.get_task(handle.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(task.attachments, vec![stored.clone()]);
    assert!(task.plan.is_empty());
    // The linkage row reconstructs the byte-identical typed set.
    let facts = handle.memory_facts().unwrap();
    let row = facts
        .iter()
        .find(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == &receipt.run_id)
        .expect("durable linkage row");
    let decoded = TaskRunRow::decode(&row.2).unwrap();
    assert_eq!(decoded.attachments, vec![stored.clone()]);
    assert_eq!(decoded.files, Vec::<String>::new());
    // Reopen the REAL store: the typed set survives, byte-identically.
    drop(handle);
    drop(env);
    let m2 = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = m2.get_session(parent).unwrap().unwrap();
    let task2 = h2.get_task(h2.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(task2.attachments, vec![stored.clone()]);
    assert_eq!(
        h2.attachment_bytes(&stored, 1 << 20).unwrap(),
        b"%PDF-1.4 spec"
    );

    // Image ids are structurally valid at this layer; the server DTO gates
    // delivery on the chosen model's vision capability and the adapters
    // encode the resolved bytes per wire.
    let image = h2
        .put_attachment("image/png", Some("shot.png"), b"\x89PNG")
        .unwrap();
    crate::runtime::validate_attachment_ids(&[image]).expect("image id is structurally valid");
}

// ------------------------------- FIX 2: strict durable-read semantics

/// A PRESENT-but-undecodable plan row is corrupt durable state: the
/// settlement refuses typed instead of silently treating the run as "no
/// plan". A genuinely MISSING plan row keeps the not-found policy (an
/// incomplete outcome, not an error).
#[tokio::test]
pub(crate) async fn malformed_plan_row_refuses_settlement_typed_never_no_plan() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    // (a) Not JSON at all.
    handle
        .upsert_memory_fact(crate::runtime::PLAN_ROW_KIND, "run-corrupt", "{not json")
        .unwrap();
    let err = env
        .executor
        .settle_run(RunSettlement::Orchestrated {
            parent: env.parent,
            run_id: "run-corrupt".into(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ExecError::Internal(m) if m.contains("corrupt durable state")),
        "malformed plan JSON must refuse typed: {err:?}"
    );
    // (b) Decodable JSON that is not a plan row (no `specs`).
    handle
        .upsert_memory_fact(crate::runtime::PLAN_ROW_KIND, "run-nospec", "{\"plan\":{}}")
        .unwrap();
    let err = env
        .executor
        .settle_run(RunSettlement::Orchestrated {
            parent: env.parent,
            run_id: "run-nospec".into(),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ExecError::Internal(m) if m.contains("corrupt durable state") && m.contains("specs")),
        "a plan row without specs must refuse typed: {err:?}"
    );
    // (c) MISSING row: the not-found policy — an incomplete settlement, not
    // an error and never a synthetic plan.
    let outcome = env
        .executor
        .settle_run(RunSettlement::Orchestrated {
            parent: env.parent,
            run_id: "run-missing".into(),
        })
        .await
        .expect("a missing plan row is the not-found policy");
    assert!(!outcome.complete && !outcome.completed);
}

/// A FAILED durable read is an error, never "nothing to do": with the fact
/// table gone, the post-run settlement pass refuses instead of reporting
/// success with zero runs.
#[tokio::test]
pub(crate) async fn failed_durable_read_refuses_settlement_never_nothing_happened() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    env.manager
        .store()
        .sql_execute("DROP TABLE memory_fact")
        .unwrap();
    let err = env
        .executor
        .settle_resolved_verifications(env.parent)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ExecError::Internal(m) if m.contains("durable read failed")),
        "a store failure must surface as an error: {err:?}"
    );
}

/// A poisoned active-run lock refuses WORK with the typed
/// [`PoisonedAuthority`] error: it is neither treated as "no active run"
/// (which would free a slot that may still be occupied) nor as "an active
/// run" (which would silently skip the post-run settlement).
#[tokio::test]
pub(crate) async fn poisoned_active_run_lock_refuses_work_typed() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    env.executor.poison_active_run_lock_for_test();
    // (a) The typed read API refuses with the named authority.
    let poison = env.executor.active_runs_checked().unwrap_err();
    assert_eq!(poison.authority, "active-run lock");
    assert!(poison.to_string().contains("poisoned authority"));
    // (b) The post-run settlement REFUSES (it used to swallow the poison as
    // "active" and skip the run).
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    handle
        .upsert_memory_fact("verification_attempt", "root:run-poisoned", "1")
        .unwrap();
    let err = env
        .executor
        .settle_resolved_verifications(env.parent)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ExecError::Internal(m) if m.contains("poisoned authority")),
        "settlement must refuse a poisoned lock: {err:?}"
    );
    // (c) Claiming a new orchestrated run refuses instead of panicking.
    let req = request(
        "start after poison",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &["a"]),
        ],
        &env,
    );
    let err = env.executor.start_task(env.parent, req).unwrap_err();
    assert!(
        matches!(&err, ExecError::Internal(m) if m.contains("poisoned authority")),
        "run admission must refuse a poisoned active-run lock: {err:?}"
    );
}

// ------------------------------- FIX 3: tri-state re-goal

/// FIX 3: the start request's criteria/attachments are tri-state — `None` =
/// continuation (preserve), `Some(vec![])` = explicitly clear, `Some(items)`
/// = replace; the dedicated patch wins over the legacy vector. All three
/// outcomes are proven against the durable task row, for criteria AND
/// attachments.
#[tokio::test]
pub(crate) async fn re_goal_tri_state_preserves_clears_and_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let a1 = h
        .put_attachment("text/plain", Some("one.txt"), b"one")
        .unwrap();
    let a2 = h
        .put_attachment("text/plain", Some("two.txt"), b"two")
        .unwrap();
    let item = || wi("impl", WorkKind::Implementation, &[]);

    // Request-level tri-state (pure, no drive needed).
    let mut probe = TaskRunRequest::default();
    assert_eq!(probe.effective_criteria_patch(), None);
    probe.criteria = vec!["legacy".into()];
    assert_eq!(
        probe.effective_criteria_patch(),
        Some(vec!["legacy".into()])
    );
    probe.criteria_patch = Some(vec![]);
    assert_eq!(probe.effective_criteria_patch(), Some(vec![]));
    probe.criteria_patch = Some(vec!["new".into()]);
    assert_eq!(probe.effective_criteria_patch(), Some(vec!["new".into()]));
    assert_eq!(probe.effective_attachments_patch(), None);
    probe.attachments = vec![a1.clone()];
    assert_eq!(probe.effective_attachments_patch(), Some(vec![a1.clone()]));
    probe.attachments_patch = Some(vec![]);
    assert_eq!(probe.effective_attachments_patch(), Some(vec![]));
    probe.attachments_patch = Some(vec![a2.clone()]);
    assert_eq!(probe.effective_attachments_patch(), Some(vec![a2.clone()]));

    // Run 1: seed a non-empty criteria/attachment contract.
    let mut r1 = request("goal one", vec![item()], &env);
    r1.criteria = vec!["c1".into()];
    r1.attachments = vec![a1.clone()];
    env.executor
        .start_task(env.parent, r1)
        .expect("first start");
    let seed = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(seed.acceptance_criteria, vec!["c1".to_string()]);
    assert_eq!(seed.attachments, vec![a1.clone()]);

    // Run 2: `None` on both = continuation: the durable row is preserved.
    let r2 = request("goal two", vec![item()], &env);
    assert_eq!(r2.effective_criteria_patch(), None);
    assert_eq!(r2.effective_attachments_patch(), None);
    env.executor
        .start_task(env.parent, r2)
        .expect("second start");
    let kept = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(
        kept.acceptance_criteria,
        vec!["c1".to_string()],
        "None (continuation) must preserve criteria"
    );
    assert_eq!(
        kept.attachments,
        vec![a1.clone()],
        "None (continuation) must preserve attachments"
    );

    // Run 3: `Some(vec![])` clears BOTH, even with a stale legacy vector
    // present (the patch wins).
    let mut r3 = request("goal three", vec![item()], &env);
    r3.criteria = vec!["stale".into()];
    r3.attachments = vec![a1.clone()];
    r3.criteria_patch = Some(vec![]);
    r3.attachments_patch = Some(vec![]);
    env.executor
        .start_task(env.parent, r3)
        .expect("third start");
    let cleared = h.get_task(task_id).unwrap().unwrap();
    assert!(
        cleared.acceptance_criteria.is_empty(),
        "Some([]) must clear criteria: {:?}",
        cleared.acceptance_criteria
    );
    assert!(
        cleared.attachments.is_empty(),
        "Some([]) must clear attachments: {:?}",
        cleared.attachments
    );

    // Run 4: `Some(items)` replaces BOTH.
    let mut r4 = request("goal four", vec![item()], &env);
    r4.criteria_patch = Some(vec!["c2".into()]);
    r4.attachments_patch = Some(vec![a2.clone()]);
    env.executor
        .start_task(env.parent, r4)
        .expect("fourth start");
    let replaced = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(replaced.acceptance_criteria, vec!["c2".to_string()]);
    assert_eq!(replaced.attachments, vec![a2.clone()]);
}

// =====================================================================
// (A) PROOF-BASIS FAIL-CLOSED
// =====================================================================

/// A workspace whose authority rule file is oversized: the resolver fails
/// typed and the proof basis (creation AND reuse) is refused — never
/// collapsed into "no instructions".
#[tokio::test]
pub(crate) async fn oversized_instruction_file_refuses_the_proof_basis() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    std::fs::write(
        env.owner_root.join("AGENTS.md"),
        vec![b'x'; faktor_instructions::MAX_RULE_BYTES + 1],
    )
    .unwrap();
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "hostile-tree");
    let err = env
        .executor
        .root_verification_proof_basis(&h, task_id, &prepared, &probe_run("cargo"))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("instruction basis"), "{text}");
    assert!(text.contains("unreadable"), "{text}");
    // The find-or-create path refuses too and writes NO record.
    let snapshot = prepared.candidate_snapshot.clone();
    let err = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("instruction basis"), "{err}");
    assert!(
        h.list_verification_records(task_id).unwrap().is_empty(),
        "a refused basis never mints a record"
    );
}

/// A session row that cannot be read is a typed store refusal of the basis
/// (never "no instructions").
#[tokio::test]
pub(crate) async fn unreadable_session_row_refuses_the_proof_basis() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    let (h, task_id, prepared, _criteria) = probe_task_and_prepared(&env, "store-down");
    // Make the session row unreadable at the store level (the FK graph
    // forbids deleting it): every read of the row now fails typed.
    env.manager
        .store()
        .sql_execute("ALTER TABLE session RENAME TO session_gone")
        .unwrap();
    let err = env
        .executor
        .root_verification_proof_basis(&h, task_id, &prepared, &probe_run("cargo"))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("instruction basis"), "{text}");
    assert!(text.contains("store unavailable"), "{text}");
    env.manager
        .store()
        .sql_execute("ALTER TABLE session_gone RENAME TO session")
        .unwrap();
}

/// A rule tree that changes between the two reads of ONE basis construction
/// is unstable: the basis (and therefore proof creation/reuse) is refused.
pub(crate) struct AlternatingRoots {
    pub(crate) first: std::path::PathBuf,
    pub(crate) second: std::path::PathBuf,
    pub(crate) calls: AtomicUsize,
}

impl faktor_instructions::WorkspaceRootProvider for AlternatingRoots {
    fn workspace_root(
        &self,
        _workspace_id: u64,
    ) -> Result<Option<std::path::PathBuf>, faktor_core::Error> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(if call.is_multiple_of(2) {
            self.first.clone()
        } else {
            self.second.clone()
        }))
    }
}

#[tokio::test]
pub(crate) async fn unstable_instruction_tree_refuses_the_proof_basis() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("rules-a");
    let second = dir.path().join("rules-b");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(first.join("AGENTS.md"), "rule set alpha\n").unwrap();
    std::fs::write(second.join("AGENTS.md"), "rule set beta\n").unwrap();
    let resolver = Arc::new(faktor_instructions::InstructionResolver::new(
        Arc::new(AlternatingRoots {
            first,
            second,
            calls: AtomicUsize::new(0),
        }),
        faktor_instructions::DEFAULT_RESOLVER_CACHE_ENTRIES,
    ));
    let env = open_real_tool_env_with_resolver(dir.path(), resolver);
    let (h, task_id, prepared, _criteria) = probe_task_and_prepared(&env, "moving-tree");
    let err = env
        .executor
        .root_verification_proof_basis(&h, task_id, &prepared, &probe_run("cargo"))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("instruction basis"), "{text}");
    assert!(text.contains("unstable"), "{text}");
}

/// A workspace that genuinely resolves to NO instruction tree is a VALID
/// epoch-less basis: the proof is created (and reused) normally.
#[tokio::test]
pub(crate) async fn no_applicable_instructions_is_a_valid_basis() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env =
        open_real_tool_env_with_resolver(dir.path(), faktor_instructions::no_roots_resolver());
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "no-instructions");
    let snapshot = prepared.candidate_snapshot.clone();
    let basis = env
        .executor
        .root_verification_proof_basis(&h, task_id, &prepared, &probe_run("cargo"))
        .await
        .unwrap();
    assert_eq!(
        basis.instruction_epoch, None,
        "no durable root resolves to the epoch-less basis"
    );
    let (record, digest) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert!(record.raw() > 0);
    let (reused, reused_digest) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(record, reused, "the canonical basis is replay-idempotent");
    assert_eq!(digest, reused_digest);
}

/// A record minted under the RETIRED serde-bytes digest is never reused: the
/// canonical domain-separated basis is the only reuse key.
#[tokio::test]
pub(crate) async fn a_legacy_digest_record_is_never_reused() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "legacy-digest");
    let snapshot = prepared.candidate_snapshot.clone();
    let run = probe_run("cargo");
    let basis = env
        .executor
        .root_verification_proof_basis(&h, task_id, &prepared, &run)
        .await
        .unwrap();
    // The exact legacy shape: a PASSED record at the right revision/tree that
    // covers every criterion, whose fingerprint carries the retired digest.
    let mut fingerprint =
        crate::runtime::task_executor::root_verification_fingerprint(&h, task_id, &basis).unwrap();
    fingerprint.proof_basis_digest = Some(basis.digest());
    let legacy = h
        .create_verification_record_with_evidence(
            task_id,
            Some(snapshot.clone()),
            run.criteria.clone(),
            run.checks.clone(),
            Vec::new(),
            Vec::new(),
            None,
            VerificationStatus::Passed,
            h.now_ms(),
            Some(fingerprint),
            None,
        )
        .unwrap();
    let canonical = crate::runtime::task_executor::canonical_proof_basis_digest(&basis);
    let (found, digest) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &run,
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_ne!(found, legacy, "a legacy-digest record is never reused");
    assert_eq!(digest, canonical);
    assert_eq!(
        h.get_verification_record(found)
            .unwrap()
            .unwrap()
            .environment_fingerprint
            .as_ref()
            .and_then(|f| f.proof_basis_digest.as_deref()),
        Some(canonical.as_str())
    );
}

/// The golden proof basis: a fixed value vector for the canonical digest.
pub(crate) fn golden_proof_basis() -> faktor_session::task::ProofBasis {
    use faktor_session::task::{ProofBasis, ProofBasisCheck, ProofBasisCriterion};
    ProofBasis {
        task_id: 7,
        task_revision: 3,
        task_contract_digest: "fnv1a64:0123456789abcdef".into(),
        candidate_snapshot: "aa".repeat(32),
        integration_sources_digest: "bb".repeat(32),
        changed_files_digest: "fnv1a64:fedcba9876543210".into(),
        checks: vec![ProofBasisCheck {
            check_id: "rust_check".into(),
            program: "cargo".into(),
            args: vec!["check".into(), "--workspace".into()],
        }],
        verification_impl_version: "faktor-agent/0.1.0".into(),
        tool_versions: vec![faktor_core::state::ToolVersion {
            tool: "rustc".into(),
            version: "1.92.0".into(),
        }],
        env_projection: vec![("RUSTFLAGS".into(), "<absent>".into())],
        instruction_epoch: Some(9),
        criteria: vec![ProofBasisCriterion {
            criterion_id: "c1".into(),
            binding_digest: Some("fnv1a64:0011223344556677".into()),
        }],
        reviewer_digest: Some("blake3:reviewer".into()),
        evidence_digests: vec!["blake3:evidence-1".into()],
    }
}

/// The canonical digest is domain/version separated (never incidental serde
/// bytes), TOTAL (no serialization failure path), stable under value-level
/// equality and changes under EVERY field.
#[test]
pub(crate) fn canonical_proof_basis_digest_is_domain_separated_and_field_sensitive() {
    use crate::runtime::task_executor::{
        canonical_proof_basis_digest, canonical_proof_basis_payload,
    };
    let base = golden_proof_basis();
    let digest = canonical_proof_basis_digest(&base);
    assert!(digest.starts_with("blake3:"), "{digest}");
    assert_eq!(digest, canonical_proof_basis_digest(&base.clone()));
    // The payload starts with the domain separator + the version.
    let payload = canonical_proof_basis_payload(&base);
    assert!(
        payload.starts_with(b"FAKTOR_PROOF_BASIS\0"),
        "domain separator"
    );
    assert_eq!(
        &payload[b"FAKTOR_PROOF_BASIS\0".len()..b"FAKTOR_PROOF_BASIS\0".len() + 8],
        &3u64.to_le_bytes(),
        "encoding version 3"
    );
    // Domain separation: the retired serde-bytes digest never coincides.
    assert_ne!(digest, base.digest());
    // Every field is load-bearing.
    let mutations: Vec<(&str, faktor_session::task::ProofBasis)> = vec![
        ("task_id", {
            let mut b = base.clone();
            b.task_id += 1;
            b
        }),
        ("task_revision", {
            let mut b = base.clone();
            b.task_revision += 1;
            b
        }),
        ("task_contract_digest", {
            let mut b = base.clone();
            b.task_contract_digest.push('x');
            b
        }),
        ("candidate_snapshot", {
            let mut b = base.clone();
            b.candidate_snapshot.push('x');
            b
        }),
        ("integration_sources_digest", {
            let mut b = base.clone();
            b.integration_sources_digest.push('x');
            b
        }),
        ("changed_files_digest", {
            let mut b = base.clone();
            b.changed_files_digest.push('x');
            b
        }),
        ("checks.len", {
            let mut b = base.clone();
            b.checks.clear();
            b
        }),
        ("check_id", {
            let mut b = base.clone();
            b.checks[0].check_id.push('x');
            b
        }),
        ("check program", {
            let mut b = base.clone();
            b.checks[0].program.push('x');
            b
        }),
        ("check args", {
            let mut b = base.clone();
            b.checks[0].args.push("x".into());
            b
        }),
        ("verification_impl_version", {
            let mut b = base.clone();
            b.verification_impl_version.push('x');
            b
        }),
        ("tool_versions", {
            let mut b = base.clone();
            b.tool_versions.clear();
            b
        }),
        ("tool version", {
            let mut b = base.clone();
            b.tool_versions[0].version.push('x');
            b
        }),
        ("env_projection", {
            let mut b = base.clone();
            b.env_projection.clear();
            b
        }),
        ("env value", {
            let mut b = base.clone();
            b.env_projection[0].1.push('x');
            b
        }),
        ("instruction_epoch", {
            let mut b = base.clone();
            b.instruction_epoch = None;
            b
        }),
        ("criteria", {
            let mut b = base.clone();
            b.criteria.clear();
            b
        }),
        ("criterion id", {
            let mut b = base.clone();
            b.criteria[0].criterion_id.push('x');
            b
        }),
        ("binding digest", {
            let mut b = base.clone();
            b.criteria[0].binding_digest = None;
            b
        }),
        ("reviewer_digest", {
            let mut b = base.clone();
            b.reviewer_digest = None;
            b
        }),
        ("evidence_digests", {
            let mut b = base.clone();
            b.evidence_digests.clear();
            b
        }),
    ];
    for (field, mutated) in mutations {
        assert_ne!(
            digest,
            canonical_proof_basis_digest(&mutated),
            "changing {field} must change the digest"
        );
        assert_eq!(
            canonical_proof_basis_digest(&mutated),
            canonical_proof_basis_digest(&mutated.clone()),
            "{field}: the digest is deterministic"
        );
    }
}

/// The canonical writer is TOTAL: hostile strings (NUL, astral characters,
/// megabytes of text) always produce a digest, and the length-prefixed legs
/// make concatenation ambiguities impossible.
#[test]
pub(crate) fn canonical_proof_basis_writer_is_total_and_boundary_safe() {
    use crate::runtime::task_executor::canonical_proof_basis_digest;
    let mut hostile = golden_proof_basis();
    hostile.task_contract_digest = "nul\0inside\u{1F600}".repeat(64);
    hostile.candidate_snapshot = "\u{0}\u{0}\u{0}".into();
    hostile.env_projection = vec![(String::new(), "x".repeat(1_000_000))];
    hostile.checks[0].args = vec!["\u{1F600}".repeat(1000)];
    let digest = canonical_proof_basis_digest(&hostile);
    assert_eq!(digest, canonical_proof_basis_digest(&hostile.clone()));
    // Length prefixes prevent ab|cd vs a|bcd collisions.
    let mut split_left = golden_proof_basis();
    split_left.task_contract_digest = "ab".into();
    split_left.candidate_snapshot = "c".into();
    let mut split_right = golden_proof_basis();
    split_right.task_contract_digest = "a".into();
    split_right.candidate_snapshot = "bc".into();
    assert_ne!(
        canonical_proof_basis_digest(&split_left),
        canonical_proof_basis_digest(&split_right)
    );
}

/// GOLDEN VECTOR: the canonical digest of the fixed basis is frozen. Any
/// encoding change (including the version bump) must change this value.
#[test]
pub(crate) fn canonical_proof_basis_golden_digest_vector() {
    use crate::runtime::task_executor::canonical_proof_basis_digest;
    assert_eq!(
        canonical_proof_basis_digest(&golden_proof_basis()),
        "blake3:e30343f28fac5f34f8ba7dd429650225493f2d07d94ef9d49d12ac87ed989c97"
    );
}

/// A poisoned completion-step POLICY lock refuses configuration with the
/// typed [`PoisonedAuthority`] error — it never half-applies a new
/// commit/push/PR policy — while the derived read paths recover (poison
/// cleared, cached runner dropped) instead of wedging later runs.
#[tokio::test]
pub(crate) async fn poisoned_completion_step_policy_refuses_mutations_typed() {
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let before = env.executor.completion_steps_config();
    // Poison exactly as a panicking writer would.
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = env.executor.completion_steps.lock().unwrap();
        panic!("poison the completion-step policy (test seam)");
    }));
    assert!(poisoned.is_err(), "the poisoner must unwind");
    assert!(env.executor.completion_steps.is_poisoned());

    // AUTHORITY/POLICY state => typed refusal: the policy mutation is
    // refused and the stored config is untouched.
    let mut changed = before.clone();
    changed.remote = "hostile-origin".into();
    let err = env
        .executor
        .configure_completion_steps(changed)
        .expect_err("a poisoned policy lock must refuse the configuration typed");
    assert!(
        matches!(&err, ExecError::Internal(m)
            if m.contains("poisoned authority") && m.contains("completion-step policy lock")),
        "the refusal must name the poisoned authority: {err:?}"
    );
    assert_eq!(env.executor.completion_steps_config(), before);

    // The recovery path clears the poison (planning reads keep serving) and
    // a later configuration is applied whole.
    assert!(!env.executor.completion_steps.is_poisoned());
    let mut next = before.clone();
    next.remote = "origin-2".into();
    env.executor
        .configure_completion_steps(next.clone())
        .unwrap();
    assert_eq!(env.executor.completion_steps_config(), next);
}

// ------------------------------------------------- worker-plane placement seam
// Adversarial tests of the ADDITIVE placement seam (crates/orchestrator/src/
// placement.rs): disabled parity, local decisions, remote decisions that
// start nothing locally, and a broken seam that refuses typed instead of
// silently running locally.

use crate::placement::{PlacementDecision, PlacementSpec, WorkerPlacement, WorkerPlacementSeam};

/// One recording seam: answers a scripted decision and counts/captures
/// every consultation.
pub(crate) struct RecordingSeam {
    pub(crate) decision: StdMutex<Result<PlacementDecision, String>>,
    pub(crate) calls: AtomicUsize,
    pub(crate) specs: StdMutex<Vec<PlacementSpec>>,
}

impl RecordingSeam {
    fn new(decision: Result<PlacementDecision, String>) -> Arc<Self> {
        Arc::new(Self {
            decision: StdMutex::new(decision),
            calls: AtomicUsize::new(0),
            specs: StdMutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl WorkerPlacementSeam for RecordingSeam {
    fn place(&self, spec: &PlacementSpec) -> Result<PlacementDecision, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.specs.lock().unwrap().push(spec.clone());
        self.decision.lock().unwrap().clone()
    }
}

/// Disabled (the default): the executor never consults any seam, the run
/// executes locally, and the generated spec is never built.
#[tokio::test]
pub(crate) async fn placement_disabled_runs_locally_and_never_consults_a_seam() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    assert!(!env.executor.worker_placement_enabled());
    let req = request(
        "disabled parity",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("local start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    assert!(receipt.op_id.is_some(), "the real local op record exists");
    wait_until(|| env.provider.count() >= 1, 30).await;
}

/// Enabled with a LOCAL decision: the seam IS consulted exactly once, the
/// receipt is a normal local one, and the run executes locally.
#[tokio::test]
pub(crate) async fn placement_enabled_local_decision_executes_locally_once() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    let seam = RecordingSeam::new(Ok(PlacementDecision::Local));
    env.executor
        .set_worker_placement(WorkerPlacement::enabled(seam.clone()));
    assert!(env.executor.worker_placement_enabled());
    let req = request(
        "local decision",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("local start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    assert_eq!(seam.calls(), 1, "exactly one placement consultation");
    let specs = seam.specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].kind, "in_session");
    assert_eq!(specs[0].payload_digest.len(), 64, "bound to the exact goal");
    assert!(specs[0]
        .job_key
        .starts_with(&format!("session-{}-goal-", env.parent.raw())));
    wait_until(|| env.provider.count() >= 1, 30).await;
}

/// Enabled with a REMOTE decision: the receipt names the remote job/lease
/// and NOTHING local starts — no provider call, no active run, no task row.
#[tokio::test]
pub(crate) async fn placement_enabled_remote_decision_starts_nothing_locally() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    let seam = RecordingSeam::new(Ok(PlacementDecision::Remote {
        job_id: "job_remote_1".into(),
        worker_id: "wrk_9".into(),
        generation: 1,
        lease_id: "lease_remote_1".into(),
    }));
    env.executor
        .set_worker_placement(WorkerPlacement::enabled(seam.clone()));
    let task_id = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .task_id()
        .unwrap();
    let req = request(
        "remote decision",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("remote start");
    assert_eq!(receipt.mode, TaskRunMode::Remote);
    assert_eq!(receipt.run_id, "job_remote_1");
    assert!(receipt.queued, "a remote run is queued for its worker");
    assert!(receipt.op_id.is_none(), "no local op was minted");
    assert!(env.executor.active_runs().is_empty());
    assert!(
        env.manager
            .get_session(env.parent)
            .unwrap()
            .unwrap()
            .get_task(task_id)
            .unwrap()
            .is_none(),
        "no local task row exists for a remotely placed run"
    );
    // Give any (incorrect) detached drive a chance to reach the provider.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.provider.count(), 0, "nothing ran locally");
}

/// Enabled with a FAILING seam: the refusal is typed and nothing starts —
/// a broken worker plane never degrades into a silent local run.
#[tokio::test]
pub(crate) async fn placement_enabled_failure_refuses_typed_and_starts_nothing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    let seam = RecordingSeam::new(Err("worker plane store unavailable".into()));
    env.executor
        .set_worker_placement(WorkerPlacement::enabled(seam.clone()));
    let req = request(
        "broken plane",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("a failed placement must refuse");
    assert!(
        matches!(err, ExecError::PlacementRefused(ref m) if m.contains("store unavailable")),
        "typed refusal expected, got {err:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.provider.count(), 0, "nothing ran locally");
    assert_eq!(seam.calls(), 1);
}

/// A replayed start consults the seam again (the seam owns idempotency via
/// the job key) and a second remote decision never touches the local run
/// machinery either.
#[tokio::test]
pub(crate) async fn placement_replay_is_delegated_to_the_seam_job_key() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    let seam = RecordingSeam::new(Ok(PlacementDecision::Remote {
        job_id: "job_remote_2".into(),
        worker_id: "wrk_9".into(),
        generation: 1,
        lease_id: "lease_remote_2".into(),
    }));
    env.executor
        .set_worker_placement(WorkerPlacement::enabled(seam.clone()));
    let req_a = request("same goal", vec![wi("a1", WorkKind::Analysis, &[])], &env);
    let req_b = request("same goal", vec![wi("a1", WorkKind::Analysis, &[])], &env);
    let first = env.executor.start_task(env.parent, req_a).unwrap();
    let second = env.executor.start_task(env.parent, req_b).unwrap();
    assert_eq!(first.mode, TaskRunMode::Remote);
    assert_eq!(second.mode, TaskRunMode::Remote);
    let specs = seam.specs.lock().unwrap().clone();
    assert_eq!(specs.len(), 2);
    assert_eq!(
        specs[0].job_key, specs[1].job_key,
        "the same session+goal maps onto one stable job key"
    );
    assert!(specs[0].job_key.contains("goal-"));
}

// ---------------------------------------------- remote-run completion gate

use crate::remote_completion::{
    RemoteCompletionClass, RemoteCompletionOutcome, RemoteRunCompletion, RemoteRunOutcome,
    RemoteVerificationClaim,
};

pub(crate) fn remote_completion(
    env: &Env,
    digest: String,
    outcome: RemoteRunOutcome,
    self_verified: bool,
    produced_digest: Option<String>,
) -> RemoteRunCompletion {
    RemoteRunCompletion {
        parent: env.parent,
        run_id: "job_remote_1".into(),
        job_id: "job_remote_1".into(),
        generation: 1,
        kind: "in_session".into(),
        digest,
        outcome,
        claim: RemoteVerificationClaim {
            self_verified,
            produced_digest,
        },
    }
}

/// A landed self-verified read-only result settles the parent run through the
/// SAME post-run pass local runs use — and starts NOTHING locally.
#[tokio::test]
pub(crate) async fn remote_completion_settles_a_self_verified_read_only_result() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    // The parent carries a durable task row (the completion-step proof read
    // requires it); the run itself was placed remotely, so nothing local ran.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: env.parent,
        goal: "remote run goal".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let outcome = env
        .executor
        .complete_remote_run(remote_completion(
            &env,
            "a".repeat(64),
            RemoteRunOutcome::Succeeded,
            true,
            None,
        ))
        .await
        .unwrap();
    match outcome {
        RemoteCompletionOutcome::Settled { class, settlement } => {
            assert_eq!(class, RemoteCompletionClass::SelfVerifiedReadOnly);
            assert_eq!(settlement.run_id, "job_remote_1");
        }
        other => panic!("expected a settlement, got {other:?}"),
    }
    assert_eq!(
        env.provider.count(),
        0,
        "a remote completion never runs the local pipeline's child drive"
    );
}

/// A produced/mutated tree, a missing self-verification claim and a failed
/// outcome all refuse to settle: the origin must verify (fail closed).
#[tokio::test]
pub(crate) async fn remote_completion_requires_origin_verification_for_mutating_and_failed_results()
{
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    let cases = [
        // The run mutated a tree: the origin must verify it.
        remote_completion(
            &env,
            "b".repeat(64),
            RemoteRunOutcome::Succeeded,
            true,
            Some("c".repeat(64)),
        ),
        // No self-verification claim: fail closed.
        remote_completion(
            &env,
            "d".repeat(64),
            RemoteRunOutcome::Succeeded,
            false,
            None,
        ),
        // A failed outcome never completes anything.
        remote_completion(&env, "e".repeat(64), RemoteRunOutcome::Failed, true, None),
    ];
    for completion in cases {
        let outcome = env.executor.complete_remote_run(completion).await.unwrap();
        match outcome {
            RemoteCompletionOutcome::OriginVerificationRequired { class, run_id, .. } => {
                assert_eq!(class, RemoteCompletionClass::OriginVerificationRequired);
                assert_eq!(run_id, "job_remote_1");
            }
            other => panic!("expected the origin-verification requirement, got {other:?}"),
        }
    }
    assert_eq!(env.provider.count(), 0);
}

/// Malformed completions are refused typed BEFORE any settlement read.
#[tokio::test]
pub(crate) async fn remote_completion_refuses_malformed_shapes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("a"), done_script());
    let mut short_digest =
        remote_completion(&env, "ff".into(), RemoteRunOutcome::Succeeded, true, None);
    assert!(matches!(
        env.executor.complete_remote_run(short_digest.clone()).await,
        Err(ExecError::Malformed(_))
    ));
    short_digest.digest = "f".repeat(64);
    short_digest.generation = 0;
    assert!(matches!(
        env.executor.complete_remote_run(short_digest).await,
        Err(ExecError::Malformed(_))
    ));
    let mut unknown_kind = remote_completion(
        &env,
        "f".repeat(64),
        RemoteRunOutcome::Succeeded,
        true,
        None,
    );
    unknown_kind.kind = "sideways".into();
    assert!(matches!(
        env.executor.complete_remote_run(unknown_kind).await,
        Err(ExecError::Malformed(_))
    ));
    let mut bad_produced = remote_completion(
        &env,
        "f".repeat(64),
        RemoteRunOutcome::Succeeded,
        true,
        Some("zz".into()),
    );
    bad_produced.kind = "in_session".into();
    assert!(matches!(
        env.executor.complete_remote_run(bad_produced).await,
        Err(ExecError::Malformed(_))
    ));
}

// ------------------------------------------------- queue settle boundary race
// (audit: no durable pending queue item may be left with no runner)

/// Owner-direct executor over a GATED provider: the active direct turn parks
/// mid-stream, so the queue runner's budget boundary and the turn's settle
/// are deterministic.
pub(crate) struct QueueRaceFix {
    pub(crate) manager: Arc<SessionManager>,
    pub(crate) agent: Arc<AgentRuntime>,
    pub(crate) executor: Arc<TaskExecutor>,
    pub(crate) gated: Arc<GatedProvider>,
    pub(crate) parent: SessionId,
    pub(crate) isolated_root: std::path::PathBuf,
}

pub(crate) fn open_queue_race(root: &std::path::Path) -> QueueRaceFix {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let gated = Arc::new(GatedProvider {
        caps: ModelCapabilities {
            tools: true,
            parallel_tools: true,
            ..Default::default()
        },
        gate: Arc::new(tokio::sync::Notify::new()),
        open: Arc::new(AtomicUsize::new(0)),
        request_count: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(gated.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    let owner_root = root.join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "queue-race", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new_owner_direct_for_test_harness(
        &orchestrator,
        manager.clone(),
        agent.clone(),
    );
    QueueRaceFix {
        manager,
        agent,
        executor,
        gated,
        parent,
        isolated_root,
    }
}

pub(crate) fn queue_race_request(fix: &QueueRaceFix, goal: &str) -> TaskRunRequest {
    TaskRunRequest {
        goal: goal.to_string(),
        work_items: vec![wi("main", WorkKind::Implementation, &[])],
        parent_caps: read_caps(),
        isolated_root: fix.isolated_root.clone(),
        ..Default::default()
    }
}

pub(crate) fn queue_race_state(fix: &QueueRaceFix) -> faktor_core::state::AgentState {
    fix.manager
        .get_session(fix.parent)
        .unwrap()
        .unwrap()
        .state()
        .unwrap()
}

pub(crate) fn queue_done_count(handle: &faktor_session::SessionHandle) -> i64 {
    handle
        .queue_status_counts()
        .unwrap()
        .get("done")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// Relaunch view: a FRESH manager/agent/executor over the same durable store
/// (process-kill recovery), with per-call scripts.
pub(crate) fn reopen_queue_executor(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
) -> (Arc<SessionManager>, Arc<TaskExecutor>, Arc<PerCallProvider>) {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let caps = ModelCapabilities {
        tools: true,
        parallel_tools: true,
        ..Default::default()
    };
    let provider = Arc::new(PerCallProvider::new("fake", caps, scripts));
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new_owner_direct_for_test_harness(
        &orchestrator,
        manager.clone(),
        agent.clone(),
    );
    (manager, executor, provider)
}

/// Boundary race (audit): the active direct turn ends exactly as the queue
/// runner's wait budget expires. The settle path must be authoritative — the
/// pending head is claimed by exactly one runner and completes exactly once,
/// with no submit and no manual kick.
#[tokio::test]
pub(crate) async fn settle_path_kicks_a_pending_head_after_the_runner_budget_expired() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let fix = open_queue_race(dir.path());
    fix.agent.set_turn_budget_ms(150);

    // A: direct in-session run, parked in the provider gate.
    let receipt_a = fix
        .executor
        .start_task(fix.parent, queue_race_request(&fix, "A"))
        .expect("direct run A");
    assert!(!receipt_a.queued);
    // B: queues behind the active A and gets a registry-owned runner.
    let receipt_b = fix
        .executor
        .start_task(fix.parent, queue_race_request(&fix, "B"))
        .expect("queued run B");
    assert!(receipt_b.queued, "B must queue behind the active A");
    let handle = fix.manager.get_session(fix.parent).unwrap().unwrap();
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);

    // The runner's bounded wait expires while A is still parked: its own
    // registry drive disappears with B still pending (the stall shape). Its
    // closure's re-kick may leave ONE replacement registry runner parked on
    // A; that replacement also expires (nothing kicks the kick), so after
    // this wait NO runner of any kind exists for B. Only A's settle-path
    // kick can drain it.
    let queue_label = format!("tx-queue-{}", fix.parent.raw());
    wait_until(
        || {
            let live = fix.executor.live_drive_runs();
            !live
                .iter()
                .any(|l| l == &receipt_b.run_id || l == &queue_label)
        },
        60,
    )
    .await;
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        1,
        "B is durably pending after the runner's budget expired"
    );

    // A ends. The settle path (no submit, no manual kick) must give B a
    // runner; the runtime gate claims the head exactly once.
    fix.gated.open();
    wait_until(|| handle.queued_prompt_count().unwrap() == 0, 120).await;
    assert_eq!(
        queue_race_state(&fix),
        faktor_core::state::AgentState::ReadyForNextTurn
    );
    assert_eq!(fix.gated.count(), 2, "A and B each streamed exactly once");
    assert_eq!(
        queue_done_count(&handle),
        1,
        "B's row completed exactly once"
    );
    let events = handle.events_range(1, None).unwrap();
    let prompts = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
        .count();
    assert_eq!(prompts, 2, "one PromptReceived per prompt");
    let turns = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::TurnCompleted)
        .count();
    assert_eq!(turns, 2, "exactly two logical turns completed");
}

/// Relaunch/recovery after a process kill (audit): a durable pending head is
/// drained by the recovery kick on a FRESH executor over the same store —
/// with no new submit.
#[tokio::test]
pub(crate) async fn relaunch_recovery_drains_a_pending_head_without_a_new_submit() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let env = open_env(&root, done_script());
    let parent = env.parent;
    // A is the active turn; B queues behind it. Neither is ever driven: the
    // process is killed with B durably pending.
    let receipt_a = env.agent.submit(parent, "A prompt", &[]).unwrap();
    assert!(!receipt_a.queued);
    let receipt_b = env.agent.submit(parent, "B prompt", &[]).unwrap();
    assert!(receipt_b.queued);
    // Abort A in-process (its durable turn record is left non-active), then
    // drop the whole first process view.
    env.agent.abort_op(parent, Some(receipt_a.op_id)).unwrap();
    {
        let handle = env.manager.get_session(parent).unwrap().unwrap();
        assert_eq!(handle.queued_prompt_count().unwrap(), 1, "B pending");
    }
    drop(env);

    // Relaunch: a FRESH manager/agent/executor over the same durable rows.
    let (manager2, executor2, provider2) = reopen_queue_executor(
        &root,
        vec![vec![
            ScriptedResponse::Text("B done".into()),
            ScriptedResponse::End,
        ]],
    );
    let handle2 = manager2.get_session(parent).unwrap().unwrap();
    assert_eq!(
        handle2.queued_prompt_count().unwrap(),
        1,
        "B survived the relaunch durably"
    );
    // The recovery entry drains the durable marker without any submit.
    executor2.recover_pending_queues();
    wait_until(|| handle2.queued_prompt_count().unwrap() == 0, 120).await;
    assert_eq!(provider2.count(), 1, "B drove exactly once");
    assert_eq!(queue_done_count(&handle2), 1);
    let events = handle2.events_range(1, None).unwrap();
    let prompts_b = events
        .iter()
        .filter(|e| {
            e.kind == faktor_core::event::EventKind::PromptReceived
                && e.op_id == Some(receipt_b.op_id)
        })
        .count();
    assert_eq!(prompts_b, 1, "B was admitted exactly once");
    let turns_b = events
        .iter()
        .filter(|e| {
            e.kind == faktor_core::event::EventKind::TurnCompleted
                && e.op_id == Some(receipt_b.op_id)
        })
        .count();
    assert_eq!(turns_b, 1, "B completed exactly once");
}

/// F3 (adversarial): the settle-path kick computes its decision from a
/// durable read. A READ FAILURE must never be read as "no pending queue"
/// (which would skip the kick and strand a durable head): it is logged
/// typed, recorded as a durable retry marker, and the runner is still
/// spawned.
#[tokio::test]
pub(crate) async fn queue_kick_read_failure_is_loud_durable_and_never_skips_the_kick() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let env = open_env(&root, done_script());
    let parent = env.parent;
    // A durable head exists: B queues behind the never-driven A.
    let _receipt_a = env.agent.submit(parent, "A prompt", &[]).unwrap();
    let receipt_b = env.agent.submit(parent, "B prompt", &[]).unwrap();
    assert!(receipt_b.queued);
    // Corrupt the read: drop the prompt_queue table under the live store.
    {
        let conn = rusqlite::Connection::open(root.join("store").join("faktor-plus.db")).unwrap();
        conn.execute_batch("DROP TABLE prompt_queue").unwrap();
    }
    // The kick must NOT be skipped on the read error; the error is recorded
    // as a durable audit marker on the session.
    env.executor.kick_pending_queue(parent);
    let handle = env.manager.get_session(parent).unwrap().unwrap();
    wait_until(
        || {
            handle.events_range(1, None).ok().is_some_and(|events| {
                events.iter().any(|e| {
                    e.kind == faktor_core::event::EventKind::CrashDetected
                        && e.payload.as_ref().is_some_and(|p| {
                            p.get("durable_write_failure")
                                .and_then(|d| d.get("site"))
                                .and_then(|s| s.as_str())
                                == Some("task_executor.kick_pending_queue.read_head")
                        })
                })
            })
        },
        30,
    )
    .await;
}

/// Concurrent settle + queued submit (audit): racing submits and settle
/// kicks must never execute a prompt twice. Every prompt runs exactly once,
/// every durable row ends terminal, and the queue is fully drained.
#[tokio::test]
pub(crate) async fn concurrent_settle_and_queued_submit_never_double_execute() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let fix = Arc::new(open_queue_race(dir.path()));
    fix.agent.set_turn_budget_ms(100);
    let receipt_a = fix
        .executor
        .start_task(fix.parent, queue_race_request(&fix, "A"))
        .expect("direct run A");
    assert!(!receipt_a.queued);
    let receipt_b = fix
        .executor
        .start_task(fix.parent, queue_race_request(&fix, "B"))
        .expect("queued run B");
    assert!(receipt_b.queued);
    let handle = fix.manager.get_session(fix.parent).unwrap().unwrap();
    // Let every runner's bounded wait expire first (the stall shape), then
    // race the settle of A against a queued submit C and a kick storm.
    let queue_label = format!("tx-queue-{}", fix.parent.raw());
    wait_until(
        || {
            let live = fix.executor.live_drive_runs();
            !live
                .iter()
                .any(|l| l == &receipt_b.run_id || l == &queue_label)
        },
        60,
    )
    .await;

    let c_fix = fix.clone();
    let submit_c = tokio::spawn(async move {
        c_fix
            .executor
            .start_task(c_fix.parent, queue_race_request(&c_fix, "C"))
    });
    let mut kicks = Vec::new();
    for _ in 0..4 {
        let k = fix.clone();
        kicks.push(tokio::spawn(async move {
            k.executor.kick_pending_queue(k.parent)
        }));
    }
    // A settles concurrently with the submit/kicks.
    fix.gated.open();
    let receipt_c = submit_c.await.unwrap().unwrap();
    for k in kicks {
        k.await.unwrap();
    }

    wait_until(|| handle.queued_prompt_count().unwrap() == 0, 120).await;
    assert_eq!(
        fix.gated.count(),
        3,
        "A, B and C each streamed exactly once (C queued={})",
        receipt_c.queued
    );
    assert_eq!(queue_done_count(&handle), 2, "B and C both terminal");
    let events = handle.events_range(1, None).unwrap();
    let prompts = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
        .count();
    assert_eq!(prompts, 3, "one PromptReceived per prompt");
    let turns = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::TurnCompleted)
        .count();
    assert_eq!(turns, 3, "exactly three logical turns completed");
}

/// Registry refusal (audit): a settle-path kick that cannot spawn (registry
/// shut down) must leave the durable head pending and resumable — the next
/// executor's recovery drains it exactly once.
#[tokio::test]
pub(crate) async fn refused_settle_kick_leaves_the_head_resumable_for_recovery() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let env = open_env(&root, done_script());
    let parent = env.parent;
    let receipt_a = env.agent.submit(parent, "A prompt", &[]).unwrap();
    assert!(!receipt_a.queued);
    let receipt_b = env.agent.submit(parent, "B prompt", &[]).unwrap();
    assert!(receipt_b.queued);
    // Crash-equivalent: A's active turn is aborted, B stays pending, and the
    // drive registry is shut down.
    env.agent.abort_op(parent, Some(receipt_a.op_id)).unwrap();
    let report = env
        .executor
        .shutdown_drives(Duration::from_millis(50))
        .await;
    assert!(report.all_reaped(), "{report:?}");
    // The refused kick changes nothing durable.
    env.executor.kick_pending_queue(parent);
    {
        let handle = env.manager.get_session(parent).unwrap().unwrap();
        assert_eq!(
            handle.queued_prompt_count().unwrap(),
            1,
            "a refused kick must leave the durable head pending"
        );
    }
    drop(env);

    // The next executor's recovery drains the very same row.
    let (manager2, executor2, provider2) = reopen_queue_executor(
        &root,
        vec![vec![
            ScriptedResponse::Text("B done".into()),
            ScriptedResponse::End,
        ]],
    );
    let handle2 = manager2.get_session(parent).unwrap().unwrap();
    assert_eq!(handle2.queued_prompt_count().unwrap(), 1);
    executor2.recover_pending_queues();
    wait_until(|| handle2.queued_prompt_count().unwrap() == 0, 120).await;
    assert_eq!(provider2.count(), 1, "B drove exactly once after recovery");
    assert_eq!(queue_done_count(&handle2), 1);
}
