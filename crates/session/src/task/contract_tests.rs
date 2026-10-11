//! Engineering-contract tests for [`Task`](super::Task): wire round-trips
//! of every origin/severity/evidence kind, legacy payloads without the
//! field, adversarial bounds, and the additive durable merge
//! ([`SessionHandle::merge_task_contract`](crate::SessionHandle::merge_task_contract)).
//! Split out of `task/tests.rs` to keep every source file under the
//! shrink-only 4000-line ceiling.

use super::*;

// ---------------------------------------------------- engineering contract

/// A contract fixture covering EVERY origin, severity, evidence kind and
/// claim state, with confidence and evidence refs attached.
fn full_contract_fixture() -> EngineeringContract {
    use faktor_core::contract::{
        ClaimState, Constraint, ContractOrigin, EvidenceRef, Invariant, ProofEvidenceKind,
        RiskHypothesis, RiskSeverity,
    };
    let origins = [
        ContractOrigin::User,
        ContractOrigin::Policy,
        ContractOrigin::Native,
        ContractOrigin::Heuristic,
        ContractOrigin::External {
            provider: "example.graph".into(),
        },
    ];
    let constraints = origins
        .iter()
        .enumerate()
        .map(|(i, origin)| Constraint {
            id: format!("c1:{i:016x}"),
            statement: format!("constraint {i}"),
            origin: origin.clone(),
        })
        .collect();
    let invariants = origins
        .iter()
        .map(|origin| Invariant {
            id: derived_contract_entry_id("i1", &format!("{origin:?}")),
            statement: format!("invariant {origin:?}"),
            origin: origin.clone(),
        })
        .collect();
    let risks = [
        RiskSeverity::Low,
        RiskSeverity::Medium,
        RiskSeverity::High,
        RiskSeverity::Unknown,
    ]
    .iter()
    .map(|severity| RiskHypothesis {
        id: derived_contract_entry_id("r1", severity.severity_str()),
        description: format!("risk {}", severity.severity_str()),
        severity: *severity,
        origin: ContractOrigin::Heuristic,
    })
    .collect();
    let states = [
        ClaimState::KnownTrue,
        ClaimState::Supported,
        ClaimState::Inferred,
        ClaimState::Uncertain,
        ClaimState::Unknown,
        ClaimState::Disproven,
    ];
    let proof_obligations = states
        .iter()
        .enumerate()
        .map(|(i, state)| {
            let evidence = match i % 5 {
                0 => ProofEvidenceKind::Compile,
                1 => ProofEvidenceKind::TestExecution,
                2 => ProofEvidenceKind::ContractCheck,
                3 => ProofEvidenceKind::Review,
                _ => ProofEvidenceKind::RuntimeTrace,
            };
            let mut obligation = EngineeringContract::obligation(
                &format!("claim {i}"),
                evidence,
                ContractOrigin::User,
            );
            obligation.state = *state;
            obligation.confidence_bps = Some(i as u16 * 1000);
            obligation.evidence_refs.push(EvidenceRef {
                kind: evidence,
                source: format!("source {i}"),
                detail: format!("detail {i}"),
            });
            obligation
        })
        .collect();
    EngineeringContract {
        constraints,
        invariants,
        risks,
        proof_obligations,
    }
}

#[test]
fn contract_task_and_contract_json_round_trip_every_variant() {
    let contract = full_contract_fixture();
    let task = Task {
        task_id: TaskId::new(7),
        session_id: SessionId::new(9),
        goal: "migrate authentication".into(),
        contract: contract.clone(),
        ..Default::default()
    };
    let encoded = serde_json::to_string(&task).unwrap();
    assert_eq!(serde_json::from_str::<Task>(&encoded).unwrap(), task);
    assert_eq!(
        serde_json::from_str::<EngineeringContract>(&serde_json::to_string(&contract).unwrap())
            .unwrap(),
        contract
    );
    // The wire shape exposes every origin, severity, evidence kind and
    // claim state (clients render provenance and disclosure from THIS DTO).
    let value = serde_json::to_value(&contract).unwrap();
    assert_eq!(value["constraints"].as_array().unwrap().len(), 5);
    assert_eq!(value["invariants"].as_array().unwrap().len(), 5);
    assert_eq!(value["risks"].as_array().unwrap().len(), 4);
    assert_eq!(value["proof_obligations"].as_array().unwrap().len(), 6);
    let origins: Vec<&serde_json::Value> = value["constraints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["origin"])
        .collect();
    for origin in ["user", "policy", "native", "heuristic"] {
        assert!(
            origins.contains(&&serde_json::Value::String(origin.into())),
            "{origin} must serialize as a plain snake_case string: {origins:?}"
        );
    }
    assert!(
        origins
            .iter()
            .any(|o| o.pointer("/external/provider").and_then(|p| p.as_str())
                == Some("example.graph")),
        "the external origin keeps its named provider: {origins:?}"
    );
    for severity in ["low", "medium", "high", "unknown"] {
        assert!(value["risks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["severity"] == severity));
    }
    for evidence in [
        "compile",
        "test_execution",
        "contract_check",
        "review",
        "runtime_trace",
    ] {
        assert!(value["proof_obligations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["evidence"] == evidence));
    }
    for state in [
        "known_true",
        "supported",
        "inferred",
        "uncertain",
        "unknown",
        "disproven",
    ] {
        assert!(value["proof_obligations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["state"] == state));
    }
    assert_eq!(value["proof_obligations"][1]["confidence_bps"], 1000);
    assert_eq!(
        value["proof_obligations"][1]["evidence_refs"][0]["kind"],
        "test_execution"
    );
}

#[test]
fn legacy_task_payload_without_contract_decodes_to_the_empty_contract() {
    // The exact payload a pre-contract writer produced (the field absent).
    let mut legacy = serde_json::to_value(Task::default()).unwrap();
    legacy.as_object_mut().unwrap().remove("contract");
    let decoded: Task = serde_json::from_value(legacy).unwrap();
    assert!(decoded.contract.is_empty());
    // An even older minimal payload still decodes (struct-level default).
    let decoded: Task = serde_json::from_value(serde_json::json!({
        "task_id": 3,
        "session_id": 4,
        "goal": "old",
    }))
    .unwrap();
    assert!(decoded.contract.is_empty());
}

#[test]
fn hostile_contracts_are_refused_typed_and_never_written() {
    use faktor_core::contract::{
        Constraint, ContractOrigin, Invariant, ProofEvidenceKind, RiskHypothesis, RiskSeverity,
    };
    let (_d, m) = test_manager();
    let s = session(&m);
    let task_id = s.task_id().unwrap();
    let refuse = |contract: EngineeringContract, oversized: bool| {
        let mut candidate = task(&s);
        candidate.contract = contract;
        let err = s.create_task(candidate).unwrap_err();
        if oversized {
            assert!(matches!(err.kind, ErrorKind::Oversized), "{err:?}");
        } else {
            assert!(matches!(err.kind, ErrorKind::Malformed), "{err:?}");
        }
        // A refused contract never leaves a partial row behind.
        assert!(s.get_task(task_id).unwrap().is_none());
    };
    // > MAX entries.
    refuse(
        EngineeringContract {
            constraints: (0..=MAX_TASK_CONTRACT_ENTRIES)
                .map(|i| Constraint {
                    id: format!("c1:{i:016x}"),
                    statement: format!("s{i}"),
                    origin: ContractOrigin::User,
                })
                .collect(),
            ..Default::default()
        },
        true,
    );
    // Empty statement.
    refuse(
        EngineeringContract {
            invariants: vec![Invariant {
                id: "i1:0000000000000000".into(),
                statement: String::new(),
                origin: ContractOrigin::User,
            }],
            ..Default::default()
        },
        false,
    );
    // Oversized statement.
    refuse(
        EngineeringContract {
            risks: vec![RiskHypothesis {
                id: "r1:0000000000000000".into(),
                description: "x".repeat(MAX_TASK_CONTRACT_STRING_BYTES + 1),
                severity: RiskSeverity::Unknown,
                origin: ContractOrigin::User,
            }],
            ..Default::default()
        },
        true,
    );
    // Duplicate entry ids are ambiguous.
    let dup = Constraint {
        id: "c1:0000000000000000".into(),
        statement: "same".into(),
        origin: ContractOrigin::User,
    };
    refuse(
        EngineeringContract {
            constraints: vec![dup.clone(), dup],
            ..Default::default()
        },
        false,
    );
    // Confidence outside the documented 0..=10_000 band.
    let mut bad_confidence =
        EngineeringContract::obligation("c", ProofEvidenceKind::Compile, ContractOrigin::User);
    bad_confidence.confidence_bps = Some(10_001);
    refuse(
        EngineeringContract {
            proof_obligations: vec![bad_confidence],
            ..Default::default()
        },
        false,
    );
    // Every refusal above left no row; once a valid contract lands, a
    // hostile MERGE is refused typed and changes neither contract nor
    // revision.
    let mut valid = task(&s);
    valid.contract = EngineeringContract {
        proof_obligations: vec![EngineeringContract::obligation(
            "kept",
            ProofEvidenceKind::Compile,
            ContractOrigin::User,
        )],
        ..Default::default()
    };
    let created = s.create_task(valid).unwrap();
    let revision = s.task_revision(task_id).unwrap();
    let hostile = EngineeringContract {
        constraints: (0..=MAX_TASK_CONTRACT_ENTRIES)
            .map(|i| Constraint {
                id: format!("c1:{i:016x}"),
                statement: format!("s{i}"),
                origin: ContractOrigin::User,
            })
            .collect(),
        ..Default::default()
    };
    assert!(matches!(
        s.merge_task_contract(task_id, &hostile).unwrap_err(),
        TaskError::Oversized(_)
    ));
    assert_eq!(s.task_revision(task_id).unwrap(), revision);
    assert_eq!(
        s.get_task(task_id).unwrap().unwrap().contract,
        created.contract
    );
}

#[test]
fn contract_merge_unions_by_id_preserves_state_is_idempotent_and_durable() {
    use faktor_core::contract::{ClaimState, Constraint, ContractOrigin, ProofEvidenceKind};
    let (dir, m) = test_manager();
    let s = session(&m);
    let task_id = s.task_id().unwrap();
    let contract = full_contract_fixture();
    let supported = contract
        .proof_obligations
        .iter()
        .find(|o| o.state == ClaimState::Supported)
        .unwrap()
        .clone();
    let mut first = task(&s);
    first.contract = contract.clone();
    let created = s.create_task(first).unwrap();
    assert_eq!(
        created.contract, contract,
        "create_task round-trips the contract through the durable row"
    );
    let revision = s.task_revision(task_id).unwrap();

    // Idempotent replay: the identical contract adds nothing and does NOT
    // bump the revision (a replayed task start cannot invalidate records).
    let replayed = s.merge_task_contract(task_id, &contract).unwrap();
    assert_eq!(replayed.contract, contract);
    assert_eq!(s.task_revision(task_id).unwrap(), revision);

    // A later start ADDS entries. Existing entries always win by id: the
    // Supported obligation keeps its state/confidence/evidence, a duplicate
    // addition of an existing id is not copied, and duplicates WITHIN the
    // additions dedupe by id.
    let mut additions = EngineeringContract::default();
    additions.constraints.push(Constraint {
        id: derived_contract_entry_id("c1", "fresh constraint"),
        statement: "fresh constraint".into(),
        origin: ContractOrigin::User,
    });
    additions.constraints.push(contract.constraints[0].clone());
    let new_obligation = EngineeringContract::obligation(
        "fresh claim",
        ProofEvidenceKind::Compile,
        ContractOrigin::User,
    );
    additions.proof_obligations.push(new_obligation.clone());
    additions.proof_obligations.push(new_obligation);
    let merged = s.merge_task_contract(task_id, &additions).unwrap();
    assert_eq!(
        s.task_revision(task_id).unwrap().raw(),
        revision.raw() + 1,
        "adding entries bumps the revision exactly once"
    );
    assert_eq!(
        merged.contract.constraints.len(),
        contract.constraints.len() + 1
    );
    assert_eq!(
        merged.contract.proof_obligations.len(),
        contract.proof_obligations.len() + 1,
        "duplicate additions dedupe by id"
    );
    let kept = merged
        .contract
        .proof_obligations
        .iter()
        .find(|o| o.id == supported.id)
        .unwrap();
    assert_eq!(kept.state, ClaimState::Supported);
    assert_eq!(kept.confidence_bps, supported.confidence_bps);
    assert_eq!(kept.evidence_refs, supported.evidence_refs);

    // Merging into a missing task is typed NotFound; a terminal row is
    // frozen.
    assert_eq!(
        s.merge_task_contract(TaskId::new(999), &additions)
            .unwrap_err(),
        TaskError::NotFound(TaskId::new(999))
    );
    let t2 = s
        .create_task(criteria_task(&s, TaskId::new(2), vec![]))
        .unwrap();
    let r2 = s.task_revision(t2.task_id).unwrap();
    s.transition_task(t2.task_id, r2, TaskTransition::Cancel, None)
        .unwrap();
    assert!(matches!(
        s.merge_task_contract(t2.task_id, &additions).unwrap_err(),
        TaskError::TerminalTask { .. }
    ));

    // The merged contract is DURABLE: a fresh manager over the same store
    // reads it back byte-identically.
    drop(m);
    let m2 = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let s2 = m2.get_session(s.id).unwrap().unwrap();
    assert_eq!(
        s2.get_task(task_id).unwrap().unwrap().contract,
        merged.contract,
        "the contract survives a store reopen"
    );
}

#[test]
fn obligation_state_updates_are_idempotent_evidence_preserving_and_bounded() {
    use faktor_core::contract::{
        ClaimState, ContractOrigin, EvidenceRef, ProofEvidenceKind, ProofObligation,
    };
    let (_d, m) = test_manager();
    let s = session(&m);
    let task_id = s.task_id().unwrap();
    let mut seed = task(&s);
    seed.contract = EngineeringContract {
        proof_obligations: vec![EngineeringContract::obligation(
            "legacy sessions survive the migration",
            ProofEvidenceKind::TestExecution,
            ContractOrigin::User,
        )],
        ..Default::default()
    };
    let created = s.create_task(seed).unwrap();
    let obligation_id = created.contract.proof_obligations[0].id.clone();
    assert!(obligation_id.starts_with("po1:"));
    let revision = s.task_revision(task_id).unwrap();
    let reference = EvidenceRef {
        kind: ProofEvidenceKind::TestExecution,
        source: "cargo test -p faktor-session".into(),
        detail: "obligation update".into(),
    };
    let mut update: ProofObligation = created.contract.proof_obligations[0].clone();
    update.state = ClaimState::Supported;
    update.confidence_bps = Some(8_000);
    update.evidence_refs = vec![reference.clone(), reference.clone()];
    let updated = s
        .apply_task_contract_obligation_states(task_id, std::slice::from_ref(&update))
        .unwrap();
    assert_eq!(
        s.task_revision(task_id).unwrap().raw(),
        revision.raw() + 1,
        "a changed state bumps the revision exactly once"
    );
    let obligation = &updated.contract.proof_obligations[0];
    assert_eq!(obligation.state, ClaimState::Supported);
    assert_eq!(obligation.confidence_bps, Some(8_000));
    assert_eq!(
        obligation.evidence_refs,
        vec![reference.clone()],
        "evidence refs dedupe by kind+source+detail"
    );
    // The claim identity is never touched by an evaluation update.
    assert_eq!(obligation.claim, "legacy sessions survive the migration");
    assert_eq!(obligation.evidence, ProofEvidenceKind::TestExecution);
    assert_eq!(obligation.origin, ContractOrigin::User);

    // Idempotent replay: no write, no revision bump, no duplicated ref.
    let replayed = s
        .apply_task_contract_obligation_states(task_id, std::slice::from_ref(&update))
        .unwrap();
    assert_eq!(s.task_revision(task_id).unwrap().raw(), revision.raw() + 1);
    assert_eq!(replayed.contract, updated.contract);

    // An unknown id is a typed refusal with zero writes: evaluation can
    // never smuggle new obligations in.
    let mut unknown = update.clone();
    unknown.id = "po1:0000000000000000000000000000000000000000000000000000000000000000".into();
    assert!(matches!(
        s.apply_task_contract_obligation_states(task_id, &[unknown])
            .unwrap_err(),
        TaskError::Malformed(_)
    ));
    assert_eq!(s.task_revision(task_id).unwrap().raw(), revision.raw() + 1);

    // Zero task ids cannot even be constructed (`TaskId::new(0)` panics by
    // invariant); an empty update batch is a typed refusal.
    assert!(matches!(
        s.apply_task_contract_obligation_states(task_id, &[])
            .unwrap_err(),
        TaskError::Malformed(_)
    ));

    // A hostile confidence is re-checked before the write (zero writes).
    let mut hostile = update.clone();
    hostile.confidence_bps = Some(10_001);
    assert!(matches!(
        s.apply_task_contract_obligation_states(task_id, &[hostile])
            .unwrap_err(),
        TaskError::Malformed(_)
    ));
    assert_eq!(s.get_task(task_id).unwrap().unwrap(), updated);

    // A terminal row is frozen even for an evaluation update.
    let mut terminal_seed = criteria_task(&s, TaskId::new(2), vec![]);
    terminal_seed.contract = created.contract.clone();
    let t2 = s.create_task(terminal_seed).unwrap();
    let r2 = s.task_revision(t2.task_id).unwrap();
    s.transition_task(t2.task_id, r2, TaskTransition::Cancel, None)
        .unwrap();
    let mut late = t2.contract.proof_obligations[0].clone();
    late.state = ClaimState::Disproven;
    assert!(matches!(
        s.apply_task_contract_obligation_states(t2.task_id, &[late])
            .unwrap_err(),
        TaskError::TerminalTask { .. }
    ));
}

/// Snapshot-bound proof records are part of the durable obligation state:
/// the evaluation's record set is persisted verbatim, idempotent replays
/// write nothing, the row survives a store reopen, and hostile proof shapes
/// (claim mismatch, count overflow, oversized counterexample, hostile
/// binding) are refused typed before any write.
#[test]
fn snapshot_bound_proof_records_are_durable_and_hostile_shapes_refused() {
    use faktor_core::contract::{
        ClaimState, ContractOrigin, EvidenceBinding, ProofBasis, ProofEvidenceKind, ProofRecord,
        SubjectSnapshot, MAX_PROOF_RECORDS,
    };
    use faktor_core::{AcceptedStrength, Requirement};
    let (dir, m) = test_manager();
    let s = session(&m);
    let task_id = s.task_id().unwrap();
    let mut seed = task(&s);
    seed.contract = EngineeringContract {
        proof_obligations: vec![EngineeringContract::obligation_bound(
            "auth replay is rejected",
            ProofEvidenceKind::TestExecution,
            ContractOrigin::User,
            Requirement::Required,
            EvidenceBinding::TestSelector("auth::refresh::replay_is_rejected".into()),
            AcceptedStrength::Supported,
        )],
        ..Default::default()
    };
    let created = s.create_task(seed).unwrap();
    let revision = s.task_revision(task_id).unwrap();
    let mut update = created.contract.proof_obligations[0].clone();
    update.state = ClaimState::Supported;
    update.confidence_bps = Some(8_000);
    update.record_proof(ProofRecord {
        claim: update.claim.clone(),
        state: ClaimState::Supported,
        confidence_bps: 8_000,
        basis: ProofBasis::snapshot_bound(
            SubjectSnapshot::new("tm1:aaaa").with_environment("env-1"),
        ),
        evidence: vec![],
        reference: None,
        counterexample: None,
    });
    let updated = s
        .apply_task_contract_obligation_states(task_id, std::slice::from_ref(&update))
        .unwrap();
    assert_eq!(s.task_revision(task_id).unwrap().raw(), revision.raw() + 1);
    let record = &updated.contract.proof_obligations[0].proofs[0];
    assert_eq!(
        record
            .basis
            .subject
            .as_ref()
            .and_then(|s| s.environment.as_deref()),
        Some("env-1")
    );
    // Idempotent replay: no write, no revision bump.
    let replayed = s
        .apply_task_contract_obligation_states(task_id, std::slice::from_ref(&update))
        .unwrap();
    assert_eq!(s.task_revision(task_id).unwrap().raw(), revision.raw() + 1);
    assert_eq!(replayed.contract, updated.contract);

    // Hostile proof claim mismatch: refused typed, zero writes.
    let mut hostile = update.clone();
    hostile.proofs[0].claim = "another claim".into();
    assert!(matches!(
        s.apply_task_contract_obligation_states(task_id, &[hostile])
            .unwrap_err(),
        TaskError::Malformed(_)
    ));
    assert_eq!(s.get_task(task_id).unwrap().unwrap(), updated);
    // Hostile proof count overflow.
    let mut hostile = update.clone();
    hostile.proofs = (0..=MAX_PROOF_RECORDS)
        .map(|i| ProofRecord {
            claim: hostile.claim.clone(),
            state: ClaimState::Supported,
            confidence_bps: 8_000,
            basis: ProofBasis::snapshot_bound(SubjectSnapshot::new(format!("tm1:{i:04}"))),
            evidence: vec![],
            reference: None,
            counterexample: None,
        })
        .collect();
    assert!(matches!(
        s.apply_task_contract_obligation_states(task_id, &[hostile])
            .unwrap_err(),
        TaskError::Oversized(_)
    ));
    // Hostile oversized counterexample.
    let mut hostile = update.clone();
    hostile.proofs[0].counterexample = Some("x".repeat(MAX_TASK_CONTRACT_STRING_BYTES + 1));
    assert!(matches!(
        s.apply_task_contract_obligation_states(task_id, &[hostile])
            .unwrap_err(),
        TaskError::Oversized(_)
    ));
    // Hostile binding (empty target) is refused at contract validation too.
    let mut hostile_contract = updated.contract.clone();
    hostile_contract.proof_obligations[0].evidence_binding =
        EvidenceBinding::CheckIds(vec![String::new()]);
    let mut hostile_task = task(&s);
    hostile_task.task_id = TaskId::new(99);
    hostile_task.contract = hostile_contract;
    assert!(matches!(
        s.create_task(hostile_task).unwrap_err().kind,
        ErrorKind::Malformed
    ));

    // The proof record is DURABLE: a fresh manager reads it back.
    drop(m);
    let m2 = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let s2 = m2.get_session(s.id).unwrap().unwrap();
    assert_eq!(
        s2.get_task(task_id).unwrap().unwrap().contract,
        updated.contract,
        "snapshot-bound proof records survive a store reopen"
    );
}
