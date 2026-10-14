use super::*;
use crate::StoreOutcomeStore;
use faktor_router::OutcomeView;

fn step(provider: &str, model: &str, local: bool) -> RunModelStep {
    let execution_class = if local {
        ExecutionClass::Local
    } else {
        ExecutionClass::HostedEconomy
    };
    step_classed(provider, model, execution_class)
}

fn step_classed(provider: &str, model: &str, execution_class: ExecutionClass) -> RunModelStep {
    RunModelStep {
        provider: provider.into(),
        model: model.into(),
        local: execution_class.is_local(),
        execution_class,
    }
}

fn sample(
    provider: &str,
    model: &str,
    phase: RouterPhase,
    verified_success: bool,
    rework_cost_micro: u64,
    rework_turns: u64,
) -> RunVerifiedSample {
    RunVerifiedSample {
        provider: provider.into(),
        model: model.into(),
        phase,
        verified_success,
        rework_cost_micro,
        rework_turns,
    }
}

/// Build one complete settlement event matching an outcome's sequence.
fn complete_settlement(
    outcome: &TerminalEngineeringOutcome,
    spends: &[u64],
    rework: u64,
    avoided: MeasuredMoney,
) -> EconomicSettlement {
    let steps: Vec<SettlementStep> = outcome
        .model_sequence
        .iter()
        .zip(spends.iter())
        .map(|(step, spend)| SettlementStep {
            provider: step.provider.clone(),
            model: step.model.clone(),
            local: step.local,
            execution_class: step.execution_class,
            spend_micro: *spend,
        })
        .collect();
    let model: u64 = spends.iter().sum();
    EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: outcome.run_id.clone(),
        seq: 0,
        money_state: MoneyState::Complete,
        reason: "test".into(),
        model_spend_micro: Some(model),
        verification_spend_micro: Some(0),
        rework_spend_micro: Some(rework.min(model)),
        avoided_hosted: avoided,
        steps,
        settled_at_ms: outcome.completed_at_ms,
    }
}

fn pending_settlement(run_id: &str) -> EconomicSettlement {
    EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: run_id.into(),
        seq: 0,
        money_state: MoneyState::Pending,
        reason: "test pending".into(),
        model_spend_micro: None,
        verification_spend_micro: None,
        rework_spend_micro: None,
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: Vec::new(),
        settled_at_ms: 1_760_000_000_000,
    }
}

/// A fully-populated engineering record where every required field is
/// non-default.
fn full_record() -> TerminalEngineeringOutcome {
    TerminalEngineeringOutcome {
        schema_version: OUTCOME_RECORD_SCHEMA_V,
        run_id: "run-2026-0001".into(),
        session_id: 41,
        task_id: 7,
        task_class: TaskClass::Hard,
        risk_class: RiskBucket::High,
        candidate_snapshot: Some("tm1:candidate-9f00".into()),
        final_snapshot: "tm1:final-1234".into(),
        accepted: true,
        integrated: true,
        verified_first_verification_pass: false,
        repair_attempts: 1,
        model_sequence: vec![
            step("ollama", "qwen3.8", true),
            step("anthropic", "claude-opus", false),
        ],
        wall_time_ms: 12_345,
        human_interventions: 2,
        proof: ProofCoverage {
            checks_passed: 4,
            checks_required: 5,
        },
        change_scale: ChangedCodeScale {
            files: 3,
            added_lines: 120,
            removed_lines: 14,
        },
        rollback_events: 1,
        recovery_events: 1,
        final_basis: FinalVerificationBasis::DeterministicChecksAndReview,
        frontier_escalations: 1,
        completed_at_ms: 1_760_000_000_000,
        verified_samples: vec![
            sample(
                "ollama",
                "qwen3.8",
                RouterPhase::Implement,
                false,
                3_000_000,
                2,
            ),
            sample(
                "anthropic",
                "claude-opus",
                RouterPhase::Implement,
                true,
                0,
                0,
            ),
        ],
    }
}

fn fixture() -> Vec<OutcomeLedgerEntry> {
    let mut a = full_record();
    a.run_id = "r-a".into();
    a.task_class = TaskClass::Medium;
    a.risk_class = RiskBucket::Low;
    a.candidate_snapshot = None;
    a.verified_first_verification_pass = true;
    a.model_sequence = vec![step("hosted", "m-standard", false)];
    a.wall_time_ms = 1_000;
    a.human_interventions = 0;
    a.proof = ProofCoverage {
        checks_passed: 5,
        checks_required: 5,
    };
    a.rollback_events = 0;
    a.recovery_events = 0;
    a.final_basis = FinalVerificationBasis::DeterministicChecks;
    a.frontier_escalations = 0;
    a.verified_samples = vec![sample(
        "hosted",
        "m-standard",
        RouterPhase::Implement,
        true,
        0,
        0,
    )];
    let a_settlement = complete_settlement(&a, &[1_000_000], 0, MeasuredMoney::Unavailable);

    let mut b = a.clone();
    b.run_id = "r-b".into();
    b.model_sequence = vec![step("ollama", "local-7b", true)];
    b.wall_time_ms = 2_000;
    b.proof = ProofCoverage {
        checks_passed: 4,
        checks_required: 4,
    };
    b.verified_samples = vec![sample(
        "ollama",
        "local-7b",
        RouterPhase::Implement,
        true,
        0,
        0,
    )];
    let b_settlement = complete_settlement(&b, &[0], 0, MeasuredMoney::measured(250_000));
    vec![
        OutcomeLedgerEntry {
            outcome: a,
            settlement: a_settlement,
        },
        OutcomeLedgerEntry {
            outcome: b,
            settlement: b_settlement,
        },
    ]
}

#[test]
fn record_written_exactly_once_per_run_with_all_fields() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store);
    let record = full_record();
    // Steps: 0 + 3_000_000; rework: 3_000_000.
    let settlement = complete_settlement(
        &record,
        &[0, 3_000_000],
        3_000_000,
        MeasuredMoney::measured(400_000),
    );
    assert_eq!(
        ledger.record(&record, &settlement).unwrap(),
        LedgerAppend::Recorded
    );
    let read = ledger.get(&record.run_id).unwrap().unwrap();
    assert_eq!(read, record);
    assert_eq!(
        read.candidate_snapshot.as_deref(),
        Some("tm1:candidate-9f00")
    );
    assert_eq!(read.final_snapshot, "tm1:final-1234");
    assert!(read.accepted && read.integrated);
    assert!(!read.verified_first_verification_pass && read.repair_attempts == 1);
    assert_eq!(read.model_sequence.len(), 2);
    assert_eq!(read.proof.checks_passed, 4);
    assert_eq!(read.change_scale.files, 3);
    assert_eq!((read.rollback_events, read.recovery_events), (1, 1));
    assert_eq!(
        read.final_basis,
        FinalVerificationBasis::DeterministicChecksAndReview
    );
    assert_eq!(read.verified_samples.len(), 2);
    let read_settlement = ledger.settlement(&record.run_id).unwrap().unwrap();
    assert_eq!(read_settlement, settlement);
    assert_eq!(ledger.count().unwrap(), 1);
    // The identical replay is a no-op: ONE record, never two.
    assert_eq!(
        ledger.record(&record, &settlement).unwrap(),
        LedgerAppend::Replayed
    );
    assert_eq!(ledger.count().unwrap(), 1);
}

#[test]
fn legacy_v1_records_decode_with_their_historic_money() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store.clone());
    let payload = serde_json::json!({
        "schema_version": 1,
        "run_id": "run-legacy",
        "session_id": 3,
        "task_id": 4,
        "task_class": "medium",
        "risk_class": "low",
        "candidate_snapshot": null,
        "final_snapshot": "tm1:legacy",
        "accepted": true,
        "integrated": true,
        "verified_first_attempt": true,
        "repair_attempts": 0,
        "model_sequence": [
            {"provider": "ollama", "model": "local-7b", "local": true, "spend_micro": 0},
        ],
        "model_spend_micro": 0,
        "verification_spend_micro": 0,
        "rework_spend_micro": 0,
        "avoided_hosted_micro": 0,
        "wall_time_ms": 100,
        "human_interventions": 0,
        "proof": {"checks_passed": 1, "checks_required": 1},
        "change_scale": {"files": 1, "added_lines": 1, "removed_lines": 0},
        "rollback_events": 0,
        "recovery_events": 0,
        "final_basis": "deterministic_checks",
        "frontier_escalations": 0,
        "completed_at_ms": 1_760_000_000_000i64,
        "verified_samples": [],
    })
    .to_string();
    let row = EngineeringOutcomeRow {
        run_id: "run-legacy".into(),
        session_id: 3,
        task_id: 4,
        payload_bytes: payload.len() as i64,
        payload,
        completed_at_ms: 1_760_000_000_000,
        verified: true,
        verified_first_pass: true,
        integrated: true,
        local_used: true,
        frontier_used: false,
        wall_time_ms: 100,
        human_interventions: 0,
        rollback_events: 0,
        recovery_events: 0,
        repair_attempts: 0,
        risk_class: "low".into(),
        changed_files: 1,
        changed_lines: 1,
        checks_passed: 1,
        checks_required: 1,
    };
    let settlement_row = EconomicSettlementRow {
        run_id: "run-legacy".into(),
        seq: 0,
        schema_version: 1,
        state: "complete".into(),
        reason: "legacy".into(),
        model_spend_micro: Some(0),
        verification_spend_micro: Some(0),
        rework_spend_micro: Some(0),
        avoided_hosted_micro: None,
        avoided_hosted_measured: false,
        steps_json: "[]".into(),
        settled_at_ms: 0,
    };
    store
        .engineering_outcome_append(&row, &[], &settlement_row)
        .unwrap();
    let read = ledger.get("run-legacy").unwrap().unwrap();
    assert_eq!(read.run_id, "run-legacy");
    assert!(read.verified());
    // The historic v1 `avoided_hosted_micro == 0` was documented as
    // UNMEASURED: the synthesized projection types it unavailable.
    let settlement = ledger.settlement("run-legacy").unwrap().unwrap();
    assert_eq!(settlement.money_state, MoneyState::Complete);
    assert_eq!(settlement.model_spend_micro, Some(0));
    assert_eq!(settlement.avoided_hosted, MeasuredMoney::Unavailable);
}

/// P2 semantics: the record's flag is a VERIFICATION-pass measure, never
/// an implementation-attempt claim. This run makes TWO durable
/// implementation/model attempts before its first verification (two
/// settled provider-call attempts, each with its own physical attempt
/// identity and reservation) and then its single verification attempt
/// passes. The honest record says exactly that: the FIRST verification
/// attempt passed (`verified_first_verification_pass == true`,
/// `repair_attempts == 0`) while the model sequence shows multiple
/// implementation attempts — the durable facts cannot establish
/// "deterministic verification passed on the first implementation
/// attempt", so the renamed field claims only what is provable.
#[tokio::test]
async fn multiple_implementation_attempts_before_one_passing_verification_are_a_verification_pass()
{
    use crate::VerificationStatus;
    use faktor_core::id::OpId;
    use faktor_core::model::ExecutionClass;
    use faktor_core::op::ModelCallAttempt;
    use faktor_core::state::{CheckExecution, FileStateEvidence, TaskState, TaskTransition};
    use faktor_session::{
        BudgetAuthority as _, DurableBudgetLedger, SessionManager, Task as SessionTask, TaskBudget,
    };

    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let workspace = manager.create_workspace("/tmp/pw-multi-attempt").unwrap();
    let handle = manager
        .create_session(workspace, "multi-attempt", "p", "m")
        .unwrap();
    let session_id = handle.id();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(SessionTask {
            task_id,
            session_id,
            goal: "multi-attempt run".into(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            contract: Default::default(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    // Reservations are legal while the run is Running (implementation);
    // the verification states forbid them (the attempt is over).
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, revision, TaskTransition::StartRunning, None)
        .unwrap();
    // TWO durable implementation/model attempts before verification: two
    // settled reservations, each keyed by its own physical attempt
    // identity, each attributed to its own completed provider-call row
    // (a local attempt at zero cost, then a hosted one).
    let budget = DurableBudgetLedger::new(manager.clone());
    for (logical, attempt, model, cost) in [
        (1u64, 2u64, "local-7b", 0u64),
        (3, 4, "hosted-big", 2_000_000),
    ] {
        let identity = ModelCallAttempt::new(OpId::new(logical), OpId::new(attempt), 0).unwrap();
        let reservation = budget
            .reserve_attempt(
                session_id,
                task_id,
                identity,
                cost.max(1),
                None,
                ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        handle
            .record_provider_call_attempt(
                identity,
                Some(reservation),
                "p",
                model,
                "completed",
                Some(10),
                Some(5),
                None,
            )
            .unwrap();
        budget
            .settle_usage(session_id, reservation, 10, 0, 0, 5, Some(cost), None)
            .await
            .unwrap();
    }
    // The implementation is done: the run requests and enters
    // verification, and its FIRST verification attempt passes.
    for transition in [
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let revision = handle.task_revision(task_id).unwrap();
        handle
            .transition_task(task_id, revision, transition, None)
            .unwrap();
    }
    handle
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![CheckExecution {
                check: "cargo test".into(),
                program: "cargo".into(),
                args: vec!["test".into()],
                category: "test".into(),
                required: true,
                status: VerificationStatus::Passed,
                started_ms: now,
                finished_ms: Some(now + 1),
                exit: Some(0),
                summary: None,
            }],
            vec![FileStateEvidence {
                path: "src/lib.rs".into(),
                digest_hex: "0".repeat(64),
                size: 6,
            }],
            vec![],
            None,
            VerificationStatus::Passed,
            now,
        )
        .unwrap();
    let record_id = handle
        .list_verification_records(task_id)
        .unwrap()
        .last()
        .unwrap()
        .record_id;
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .complete_verified_task(task_id, revision, record_id)
        .unwrap();

    let run_id = session_terminal_run_id(session_id);
    let ledger = OutcomeLedger::new(manager.store());
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        Some(LedgerAppend::Recorded)
    );
    let record = ledger.get(&run_id).unwrap().unwrap();
    assert!(record.verified(), "{record:?}");
    assert_eq!(
        record.model_sequence.len(),
        2,
        "TWO durable implementation/model attempts precede the verification: {record:?}"
    );
    assert_eq!(record.model_sequence[0].model, "local-7b");
    assert_eq!(record.model_sequence[1].model, "hosted-big");
    assert_eq!(record.repair_attempts, 0, "no verification attempt failed");
    assert!(
        record.verified_first_verification_pass,
        "the FIRST verification attempt passed: {record:?}"
    );
}

/// P1 end-to-end classification: the derived step classes come from the
/// reservation's DURABLE route-time authority, never from a cost number.
/// A LocalZero reservation IS local; a HOSTED zero-price route (free
/// tier/promotion) is NOT local and stays `unknown` — counted as neither
/// local nor hosted — and the frontier rate stays unmeasured while class
/// authority is incomplete.
#[tokio::test]
async fn derivation_classifies_from_durable_route_authority_not_from_cost() {
    use crate::VerificationStatus;
    use faktor_core::id::OpId;
    use faktor_core::model::{PriceAuthority, PriceQuote, PricingSnapshot};
    use faktor_core::op::ModelCallAttempt;
    use faktor_core::state::{CheckExecution, FileStateEvidence, TaskState, TaskTransition};
    use faktor_session::{
        BudgetAuthority as _, DurableBudgetLedger, SessionManager, Task as SessionTask, TaskBudget,
    };

    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let workspace = manager.create_workspace("/tmp/pw-class").unwrap();
    let handle = manager
        .create_session(workspace, "class", "p", "m")
        .unwrap();
    let session_id = handle.id();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(SessionTask {
            task_id,
            session_id,
            goal: "class authority run".into(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            contract: Default::default(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, revision, TaskTransition::StartRunning, None)
        .unwrap();
    let budget = DurableBudgetLedger::new(manager.clone());
    // (1) An authoritative LOCAL zero: class `local`.
    // (2) A HOSTED zero-price free tier: a numeric zero is NOT locality
    //     — its class is `unknown` (no hosted-tier authority names it).
    let attempts = [
        (
            1u64,
            2u64,
            "ollama",
            "local-7b",
            PricingSnapshot::local_zero(1, "ollama".into()),
            ExecutionClass::from_price_authority(PriceAuthority::LocalZero),
        ),
        (
            3,
            4,
            "hosted",
            "free-tier",
            PricingSnapshot::exact(PriceQuote::ZERO, 1, "hosted-free-tier".into()),
            ExecutionClass::from_price_authority(PriceAuthority::Exact),
        ),
    ];
    for (logical, physical, provider, model, snapshot, class) in attempts {
        let identity = ModelCallAttempt::new(OpId::new(logical), OpId::new(physical), 0).unwrap();
        let reservation = budget
            .reserve_attempt(session_id, task_id, identity, 1, Some(snapshot), class)
            .await
            .unwrap();
        handle
            .record_provider_call_attempt(
                identity,
                Some(reservation),
                provider,
                model,
                "completed",
                Some(10),
                Some(5),
                None,
            )
            .unwrap();
        budget
            .settle_usage(session_id, reservation, 10, 0, 0, 5, Some(0), None)
            .await
            .unwrap();
    }
    for transition in [
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let revision = handle.task_revision(task_id).unwrap();
        handle
            .transition_task(task_id, revision, transition, None)
            .unwrap();
    }
    handle
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![CheckExecution {
                check: "cargo test".into(),
                program: "cargo".into(),
                args: vec!["test".into()],
                category: "test".into(),
                required: true,
                status: VerificationStatus::Passed,
                started_ms: now,
                finished_ms: Some(now + 1),
                exit: Some(0),
                summary: None,
            }],
            vec![FileStateEvidence {
                path: "src/lib.rs".into(),
                digest_hex: "0".repeat(64),
                size: 6,
            }],
            vec![],
            None,
            VerificationStatus::Passed,
            now,
        )
        .unwrap();
    let record_id = handle
        .list_verification_records(task_id)
        .unwrap()
        .last()
        .unwrap()
        .record_id;
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .complete_verified_task(task_id, revision, record_id)
        .unwrap();

    let run_id = session_terminal_run_id(session_id);
    let ledger = OutcomeLedger::new(manager.store());
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        Some(LedgerAppend::Recorded)
    );
    let record = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(record.model_sequence.len(), 2, "{record:?}");
    assert_eq!(
        record.model_sequence[0].execution_class,
        ExecutionClass::Local,
        "the LocalZero reservation IS local"
    );
    assert!(record.model_sequence[0].local);
    assert_eq!(
        record.model_sequence[1].execution_class,
        ExecutionClass::Unknown,
        "the hosted zero-price route is NOT local and has no tier authority"
    );
    assert!(
        !record.model_sequence[1].local,
        "a zero-cost hosted model is NOT local"
    );
    let metrics = ledger.metrics().unwrap().metrics;
    assert_eq!(metrics.local_runs, 1, "{metrics:?}");
    assert_eq!(
        metrics.hosted_only_runs, 0,
        "the unknown-class step is counted as NEITHER local nor hosted"
    );
    assert_eq!(
        metrics.frontier_measurement,
        FrontierMeasurement::Partial,
        "one run is classed, the other is not"
    );
    assert_eq!(metrics.frontier_escalation_ppm, None);
}

/// The control for the verification-pass semantics: when the run's first
/// verification attempt FAILS and a later one passes, the flag is false
/// and the failed attempt is counted as one repair.
#[tokio::test]
async fn a_failed_first_verification_attempt_is_not_a_first_verification_pass() {
    use crate::VerificationStatus;
    use faktor_core::state::{CheckExecution, TaskState, TaskTransition};
    use faktor_session::{SessionManager, Task as SessionTask, TaskBudget};

    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let workspace = manager.create_workspace("/tmp/pw-repair").unwrap();
    let handle = manager
        .create_session(workspace, "repair", "p", "m")
        .unwrap();
    let session_id = handle.id();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(SessionTask {
            task_id,
            session_id,
            goal: "repair run".into(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            contract: Default::default(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    for transition in [
        TaskTransition::StartRunning,
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let revision = handle.task_revision(task_id).unwrap();
        handle
            .transition_task(task_id, revision, transition, None)
            .unwrap();
    }
    let check = |status: VerificationStatus, at: i64| CheckExecution {
        check: "cargo test".into(),
        program: "cargo".into(),
        args: vec!["test".into()],
        category: "test".into(),
        required: true,
        status,
        started_ms: at,
        finished_ms: Some(at + 1),
        exit: Some(if status == VerificationStatus::Passed {
            0
        } else {
            1
        }),
        summary: None,
    };
    // First verification attempt: FAILED.
    handle
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![check(VerificationStatus::Failed, now)],
            vec![],
            vec![],
            None,
            VerificationStatus::Failed,
            now,
        )
        .unwrap();
    // The runtime sends the run back for repair, then re-verifies.
    for transition in [TaskTransition::Reverify, TaskTransition::StartVerification] {
        let revision = handle.task_revision(task_id).unwrap();
        handle
            .transition_task(task_id, revision, transition, None)
            .unwrap();
    }
    handle
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![check(VerificationStatus::Passed, now + 10)],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            now + 10,
        )
        .unwrap();
    let record_id = handle
        .list_verification_records(task_id)
        .unwrap()
        .last()
        .unwrap()
        .record_id;
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .complete_verified_task(task_id, revision, record_id)
        .unwrap();

    let run_id = session_terminal_run_id(session_id);
    let ledger = OutcomeLedger::new(manager.store());
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        Some(LedgerAppend::Recorded)
    );
    let record = ledger.get(&run_id).unwrap().unwrap();
    assert!(record.verified(), "{record:?}");
    assert_eq!(record.repair_attempts, 1);
    assert!(
        !record.verified_first_verification_pass,
        "the first verification attempt failed, so this is not a first \
         verification pass: {record:?}"
    );
}

/// Durable payload compatibility: new writes carry the honest
/// `verified_first_verification_pass` tag; the retired
/// `verified_first_attempt` tag still decodes into the same honest
/// field (the legacy v1 shape decode is covered separately).
#[test]
fn the_engineering_payload_writes_the_honest_tag_and_accepts_the_legacy_alias() {
    let record = full_record();
    let value = serde_json::to_value(&record).unwrap();
    assert!(
        value.get("verified_first_verification_pass").is_some(),
        "new payloads write the honest tag: {value}"
    );
    assert!(
        value.get("verified_first_attempt").is_none(),
        "new payloads never write the retired claim"
    );
    let mut legacy_tagged = value;
    let obj = legacy_tagged.as_object_mut().unwrap();
    let flag = obj.remove("verified_first_verification_pass").unwrap();
    obj.insert("verified_first_attempt".into(), flag);
    let decoded: TerminalEngineeringOutcome = serde_json::from_value(legacy_tagged).unwrap();
    assert_eq!(decoded, record, "the legacy tag decodes to the same record");
}

/// P1 payload honesty: every new step writes its durable
/// `execution_class`; a pre-class payload (no key) decodes to `unknown`
/// — never a guessed local or hosted class.
#[test]
fn new_steps_write_the_execution_class_and_legacy_payloads_decode_unknown() {
    let record = full_record();
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["model_sequence"][0]["execution_class"], "local");
    assert_eq!(
        value["model_sequence"][1]["execution_class"],
        "hosted_economy"
    );
    let mut legacy = value;
    for step in legacy["model_sequence"].as_array_mut().unwrap() {
        step.as_object_mut().unwrap().remove("execution_class");
    }
    let decoded: TerminalEngineeringOutcome = serde_json::from_value(legacy).unwrap();
    assert!(
        decoded
            .model_sequence
            .iter()
            .all(|s| s.execution_class == ExecutionClass::Unknown),
        "a pre-class payload decodes to unknown, never a fabricated class"
    );
    // The recorded local bit survives the decode (legacy compat), but
    // the class authority is absent.
    assert!(decoded.model_sequence[0].local);
    assert!(!decoded.model_sequence[1].local);
}

/// The wire legacy semantics: `local` is CLASS-DERIVED (true only for a
/// durably local step), and a classless step whose pre-class payload
/// recorded a local execution exposes that recorded bit as the documented
/// `legacyLocal` marker — never as a class.
#[test]
fn wire_local_is_class_derived_and_legacy_local_marks_pre_class_rows() {
    // A durable local step: wire-local true, no legacy marker.
    let local = step_classed("ollama", "lm", ExecutionClass::Local);
    assert!(local.wire_local() && !local.legacy_local());
    // A durable hosted step: never local, no legacy marker.
    for class in [
        ExecutionClass::HostedEconomy,
        ExecutionClass::HostedBalanced,
        ExecutionClass::Frontier,
    ] {
        let hosted = step_classed("hosted", "m", class);
        assert!(!hosted.wire_local() && !hosted.legacy_local());
    }
    // A legacy pre-class local row: the wire `local` stays FALSE
    // (class-derived) while the recorded bit rides `legacyLocal`.
    let legacy_local = RunModelStep {
        provider: "ollama".into(),
        model: "lm".into(),
        local: true,
        execution_class: ExecutionClass::Unknown,
    };
    assert!(!legacy_local.wire_local(), "unknown is never wire-local");
    assert!(legacy_local.legacy_local());
    // A classless non-local row (legacy or unpriced): neither marker.
    let legacy_hosted = step_classed("hosted", "m", ExecutionClass::Unknown);
    assert!(!legacy_hosted.wire_local() && !legacy_hosted.legacy_local());
}

/// Hosted-only and frontier coverage denominators are first-class fold
/// outputs: runs with steps vs classed runs are distinct cohorts, and an
/// empty-sequence run belongs to neither.
#[test]
fn runs_with_steps_and_classed_coverage_are_folded_denominators() {
    // One classed run with steps, one classless run WITH steps, and one
    // run with NO steps at all.
    let mut entries = Vec::new();
    let mut classed = full_record();
    classed.run_id = "classed".into();
    classed.model_sequence = vec![step("hosted", "m", false)];
    // This test pins the COHORT denominators, not the frontier counters:
    // clear the inherited recorded escalation so `frontier_*` asserts the
    // classed-coverage rule instead of the escalation itself.
    classed.frontier_escalations = 0;
    let classed_settlement = complete_settlement(&classed, &[100], 0, MeasuredMoney::Unavailable);
    entries.push(OutcomeLedgerEntry {
        outcome: classed,
        settlement: classed_settlement,
    });
    let mut classless = full_record();
    classless.run_id = "classless".into();
    classless.model_sequence = vec![step_classed("hosted", "m", ExecutionClass::Unknown)];
    classless.frontier_escalations = 0;
    classless.verified_samples.clear();
    let classless_settlement =
        complete_settlement(&classless, &[200], 0, MeasuredMoney::Unavailable);
    entries.push(OutcomeLedgerEntry {
        outcome: classless,
        settlement: classless_settlement,
    });
    let mut stepless = full_record();
    stepless.run_id = "stepless".into();
    stepless.model_sequence = Vec::new();
    stepless.frontier_escalations = 0;
    stepless.verified_samples.clear();
    let stepless_settlement = complete_settlement(&stepless, &[], 0, MeasuredMoney::Unavailable);
    entries.push(OutcomeLedgerEntry {
        outcome: stepless,
        settlement: stepless_settlement,
    });
    let m = OutcomeLedgerMetrics::compute(&entries);
    assert_eq!(m.runs, 3);
    assert_eq!(
        m.runs_with_steps, 2,
        "the stepless run carries no rate evidence"
    );
    assert_eq!(m.hosted_only_runs, 1);
    assert_eq!(
        m.hosted_only_ppm, 500_000,
        "1 hosted-only of 2 runs WITH steps (never 1/3)"
    );
    assert_eq!(m.frontier_classed_runs, 1);
    assert_eq!(
        m.frontier_escalation_classed_runs, 0,
        "no recorded escalation here; a stepless run never joins the \
         classed cohort (the vacuous `all()` must not count it, never 1)"
    );
    assert_eq!(m.frontier_measurement, FrontierMeasurement::Partial);
    // An EMPTY model sequence proves no class authority: a stepless run
    // with a recorded frontier escalation is a frontier run, but it can
    // never enter the classed cohort (the vacuous `all()` must not count
    // it) — and with no known class anywhere the rate stays unmeasured.
    let mut escalated_stepless = full_record();
    escalated_stepless.run_id = "escalated-stepless".into();
    escalated_stepless.model_sequence = Vec::new();
    escalated_stepless.verified_samples.clear();
    let escalated_settlement =
        complete_settlement(&escalated_stepless, &[], 0, MeasuredMoney::Unavailable);
    let m = OutcomeLedgerMetrics::compute(&[OutcomeLedgerEntry {
        outcome: escalated_stepless,
        settlement: escalated_settlement,
    }]);
    assert_eq!(m.frontier_escalation_runs, 1);
    assert_eq!(
        m.frontier_classed_runs, 0,
        "a stepless run is never fully classed"
    );
    assert_eq!(m.frontier_escalation_classed_runs, 0);
    assert_eq!(m.frontier_escalation_ppm, None);
    assert_eq!(m.frontier_measurement, FrontierMeasurement::Unmeasured);
}

#[test]
fn metrics_fixture_includes_local_zero_cost_substitution_and_rework() {
    let mut entries = fixture();
    // C: local-then-hosted repair (rework), one human intervention.
    let mut c = entries[0].outcome.clone();
    c.run_id = "r-c".into();
    c.verified_first_verification_pass = false;
    c.repair_attempts = 1;
    c.model_sequence = vec![
        step("ollama", "local-7b", true),
        step("hosted", "m-strong", false),
    ];
    c.wall_time_ms = 5_000;
    c.human_interventions = 1;
    c.proof = ProofCoverage {
        checks_passed: 6,
        checks_required: 6,
    };
    c.frontier_escalations = 1;
    let c_settlement =
        complete_settlement(&c, &[0, 3_000_000], 3_000_000, MeasuredMoney::Unavailable);
    // D: rolled back, review-only basis (NOT a verified change).
    let mut d = entries[0].outcome.clone();
    d.run_id = "r-d".into();
    d.integrated = false;
    d.model_sequence = vec![step("hosted", "m-strong", false)];
    d.wall_time_ms = 3_000;
    d.proof = ProofCoverage::default();
    d.rollback_events = 2;
    d.recovery_events = 1;
    d.final_basis = FinalVerificationBasis::ReviewOnly;
    d.frontier_escalations = 1;
    let d_settlement = complete_settlement(&d, &[500_000], 0, MeasuredMoney::Unavailable);
    // E: frontier escalation, deterministic checks.
    let mut e = entries[0].outcome.clone();
    e.run_id = "r-e".into();
    e.model_sequence = vec![step("hosted", "m-frontier", false)];
    e.wall_time_ms = 4_000;
    e.proof = ProofCoverage {
        checks_passed: 2,
        checks_required: 2,
    };
    e.frontier_escalations = 1;
    let e_settlement = complete_settlement(&e, &[700_000], 0, MeasuredMoney::Unavailable);
    entries.push(OutcomeLedgerEntry {
        outcome: c,
        settlement: c_settlement,
    });
    entries.push(OutcomeLedgerEntry {
        outcome: d,
        settlement: d_settlement,
    });
    entries.push(OutcomeLedgerEntry {
        outcome: e,
        settlement: e_settlement,
    });

    let m = OutcomeLedgerMetrics::compute(&entries);
    assert_eq!(m.runs, 5);
    assert_eq!(m.verified_changes, 4, "D is review-only");
    assert_eq!(m.verified_first_verification_pass_changes, 3);
    assert_eq!(m.verified_first_verification_pass_ppm, 750_000);
    assert_eq!(m.money_complete_runs, 5);
    assert_eq!(m.money_incomplete_excluded, 0);
    assert_eq!(m.verified_changes_money_complete, 4);
    // Verified spend: A 1_000_000 + B 0 + C 3_000_000 + E 700_000.
    assert_eq!(m.total_verified_spend_micro, 4_700_000);
    assert_eq!(m.successful_run_cost_micro, 1_175_000);
    assert_eq!(m.total_spend_micro, 5_200_000);
    // VCSC = TOTAL eligible cohort spend / verified changes (the failed
    // D's 500_000 is in the denominator's numerator, never dropped).
    assert_eq!(m.verified_cost_per_successful_change_micro, 1_300_000);
    assert_eq!(m.total_rework_spend_micro, 3_000_000);
    assert_eq!(m.rework_cost_ratio_ppm, 576_923);
    assert_eq!(m.local_runs, 2, "B and C used a local model");
    assert_eq!(m.local_only_runs, 1);
    assert_eq!(m.local_only_verified_changes, 1);
    assert_eq!(m.local_only_verified_ppm, 1_000_000);
    assert_eq!(m.local_final_runs, 1);
    assert_eq!(m.local_final_verified_changes, 1);
    assert_eq!(m.local_final_verified_ppm, 1_000_000);
    assert_eq!(m.local_first_runs, 2);
    assert_eq!(m.local_first_success_changes, 1);
    assert_eq!(m.local_first_success_ppm, 500_000);
    assert_eq!(m.local_to_hosted_fallback_runs, 1);
    assert_eq!(m.local_to_hosted_fallback_ppm, 500_000);
    assert_eq!(m.hosted_only_runs, 3);
    assert_eq!(m.hosted_only_ppm, 600_000);
    assert_eq!(
        m.avoided_hosted_inference_micro,
        Some(250_000),
        "only B carried a measured counterfactual; unmeasured stays unavailable"
    );
    assert_eq!(m.avoided_hosted_measured_runs, 1);
    assert_eq!(m.total_wall_time_ms, 12_000);
    assert_eq!(m.mean_time_to_verified_change_ms, 3_000);
    assert_eq!(m.human_intervention_runs, 1);
    assert_eq!(m.human_intervention_ppm, 200_000);
    assert_eq!(m.rollback_runs, 1);
    assert_eq!(m.regression_rollback_ppm, 200_000);
    assert_eq!((m.checks_passed, m.checks_required), (17, 17));
    assert_eq!(m.proof_coverage_ppm, 1_000_000);
    assert_eq!(m.frontier_escalation_runs, 3);
    assert_eq!(
        m.frontier_escalation_ppm,
        Some(600_000),
        "every fixture step carries a durable class: the rate measures for real"
    );
    assert_eq!(m.frontier_measurement, FrontierMeasurement::Measured);
}

/// P1: without durable execution-class authority the frontier escalation
/// rate is UNMEASURED — `null` with a measurement state, never a
/// fabricated 0%.
#[test]
fn unmeasured_frontier_rate_is_null_not_zero() {
    let mut entry = fixture().remove(0);
    entry.outcome.model_sequence = vec![step_classed("hosted", "m", ExecutionClass::Unknown)];
    entry.outcome.verified_samples.clear();
    entry.settlement.steps = vec![SettlementStep {
        provider: "hosted".into(),
        model: "m".into(),
        local: false,
        execution_class: ExecutionClass::Unknown,
        spend_micro: 1_000_000,
    }];
    let m = OutcomeLedgerMetrics::compute(std::slice::from_ref(&entry));
    assert_eq!(m.frontier_escalation_runs, 0);
    assert_eq!(m.frontier_escalation_ppm, None);
    assert_eq!(m.frontier_measurement, FrontierMeasurement::Unmeasured);
    // The unknown-class run is excluded from BOTH local and hosted.
    assert_eq!(m.local_runs, 0);
    assert_eq!(m.hosted_only_runs, 0);
    assert_eq!(m.runs_with_steps, 1, "the sequence still carries a step");
    assert_eq!(m.frontier_classed_runs, 0);
    assert_eq!(m.frontier_escalation_classed_runs, 0);
    // An empty scan is unmeasured too — never a real 0%.
    let empty = OutcomeLedgerMetrics::compute(&[]);
    assert_eq!(empty.frontier_escalation_ppm, None);
    assert_eq!(empty.frontier_measurement, FrontierMeasurement::Unmeasured);
}

/// P1: with PARTIAL class authority the frontier rate covers exactly the
/// fully-classed runs (the state says so); a durably-known Frontier step
/// counts as an escalation for real.
#[test]
fn partial_frontier_rate_covers_exactly_the_classed_runs() {
    let mut entries = Vec::new();
    // One fully-classed run with a Frontier step.
    let mut classed = full_record();
    classed.run_id = "classed".into();
    classed.model_sequence = vec![
        step("hosted", "m-economy", false),
        step_classed("hosted", "m-frontier", ExecutionClass::Frontier),
    ];
    let classed_settlement =
        complete_settlement(&classed, &[100, 200], 0, MeasuredMoney::Unavailable);
    entries.push(OutcomeLedgerEntry {
        outcome: classed,
        settlement: classed_settlement,
    });
    // One legacy-class run: not counted in the classed cohort.
    let mut legacy = full_record();
    legacy.run_id = "legacy".into();
    legacy.model_sequence = vec![step_classed("hosted", "m", ExecutionClass::Unknown)];
    // No recorded escalation: the escalation of interest below is the
    // durably-known Frontier STEP of the classed run.
    legacy.frontier_escalations = 0;
    legacy.verified_samples.clear();
    let legacy_settlement = complete_settlement(&legacy, &[300], 0, MeasuredMoney::Unavailable);
    entries.push(OutcomeLedgerEntry {
        outcome: legacy,
        settlement: legacy_settlement,
    });
    let m = OutcomeLedgerMetrics::compute(&entries);
    assert_eq!(m.frontier_escalation_runs, 1);
    assert_eq!(m.frontier_measurement, FrontierMeasurement::Partial);
    assert_eq!(
        m.frontier_classed_runs, 1,
        "the fully-classed run is the partial-rate denominator"
    );
    assert_eq!(
        m.frontier_escalation_classed_runs, 1,
        "the classed frontier run is the partial-rate numerator"
    );
    assert_eq!(m.runs_with_steps, 2);
    assert_eq!(
        m.frontier_escalation_ppm,
        Some(1_000_000),
        "1/1 among the fully-classed runs"
    );
}

#[test]
fn incomplete_money_is_excluded_from_exact_vcsc_and_counted() {
    let mut entries = fixture();
    // A settled pending run: engineering counts, money must NOT.
    let mut pending = entries[0].outcome.clone();
    pending.run_id = "r-pending".into();
    entries.push(OutcomeLedgerEntry {
        outcome: pending,
        settlement: pending_settlement("r-pending"),
    });
    // An unavailable run.
    let mut unavailable = entries[0].outcome.clone();
    unavailable.run_id = "r-unavailable".into();
    let mut unavailable_settlement = pending_settlement("r-unavailable");
    unavailable_settlement.money_state = MoneyState::Unavailable;
    entries.push(OutcomeLedgerEntry {
        outcome: unavailable,
        settlement: unavailable_settlement,
    });
    let m = OutcomeLedgerMetrics::compute(&entries);
    assert_eq!(m.runs, 4);
    // Engineering: the pending/unavailable runs verified too.
    assert_eq!(m.verified_changes, 4);
    assert_eq!(m.verified_first_verification_pass_changes, 4);
    // Money: only A + B participate.
    assert_eq!(m.money_complete_runs, 2);
    assert_eq!(m.money_pending_runs, 1);
    assert_eq!(m.money_unavailable_runs, 1);
    assert_eq!(m.money_incomplete_excluded, 2);
    assert_eq!(m.verified_changes_money_complete, 2);
    assert_eq!(m.total_spend_micro, 1_000_000);
    // VCSC: 1_000_000 / 2 (the complete cohort only) = 500_000.
    assert_eq!(m.verified_cost_per_successful_change_micro, 500_000);
    assert_eq!(m.successful_run_cost_micro, 500_000);
    // Only fixture B's measured counterfactual participates; the
    // pending/unavailable runs contribute nothing (never a zero).
    assert_eq!(m.avoided_hosted_inference_micro, Some(250_000));
    assert_eq!(m.avoided_hosted_measured_runs, 1);
}

#[test]
fn vcsc_counts_the_whole_attempt_cohort_not_only_the_successes() {
    // Audit example: 9 failed attempts at $10 plus 1 verified change at
    // $1 => $91 per successful verified change, NOT $1.
    let mut entries = Vec::new();
    for index in 0..9u64 {
        let mut failed = full_record();
        failed.run_id = format!("fail-{index}");
        failed.accepted = false;
        failed.integrated = false;
        failed.verified_first_verification_pass = false;
        failed.model_sequence = vec![step("hosted", "m", false)];
        failed.final_basis = FinalVerificationBasis::Unverified;
        failed.verified_samples.clear();
        let settlement = EconomicSettlement {
            schema_version: SETTLEMENT_SCHEMA_V,
            run_id: failed.run_id.clone(),
            seq: 0,
            money_state: MoneyState::Complete,
            reason: "test".into(),
            model_spend_micro: Some(10_000_000),
            verification_spend_micro: Some(0),
            rework_spend_micro: Some(10_000_000),
            avoided_hosted: MeasuredMoney::Unavailable,
            steps: vec![SettlementStep {
                provider: "hosted".into(),
                model: "m".into(),
                local: false,
                execution_class: ExecutionClass::HostedEconomy,
                spend_micro: 10_000_000,
            }],
            settled_at_ms: failed.completed_at_ms,
        };
        entries.push(OutcomeLedgerEntry {
            outcome: failed,
            settlement,
        });
    }
    let mut success = full_record();
    success.run_id = "success".into();
    success.model_sequence = vec![step("hosted", "m", false)];
    success.verified_samples.clear();
    let settlement = complete_settlement(&success, &[1_000_000], 0, MeasuredMoney::Unavailable);
    entries.push(OutcomeLedgerEntry {
        outcome: success,
        settlement,
    });
    let m = OutcomeLedgerMetrics::compute(&entries);
    assert_eq!(m.runs, 10);
    assert_eq!(m.verified_changes, 1);
    assert_eq!(m.successful_run_cost_micro, 1_000_000);
    assert_eq!(m.total_verified_spend_micro, 1_000_000);
    assert_eq!(m.total_spend_micro, 91_000_000);
    assert_eq!(m.verified_cost_per_successful_change_micro, 91_000_000);
    assert_ne!(
        m.verified_cost_per_successful_change_micro, m.successful_run_cost_micro,
        "the audit's $1-vs-$91 distinction must be visible"
    );
}

#[test]
fn money_paths_remain_exact_above_f64_precision_and_round_up() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store.clone());
    // 2^53 + 1: any f64/f32 path would silently corrupt this.
    let big = 9_007_199_254_740_993u64;
    let mut record = full_record();
    record.run_id = "run-big".into();
    record.model_sequence = vec![step("hosted", "m", false)];
    record.candidate_snapshot = None;
    record.verified_samples = vec![sample("hosted", "m", RouterPhase::Implement, true, 0, 0)];
    let settlement = EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: record.run_id.clone(),
        seq: 0,
        money_state: MoneyState::Complete,
        reason: "test".into(),
        model_spend_micro: Some(big),
        verification_spend_micro: Some(0),
        rework_spend_micro: Some(big),
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: vec![SettlementStep {
            provider: "hosted".into(),
            model: "m".into(),
            local: false,
            execution_class: ExecutionClass::HostedEconomy,
            spend_micro: big,
        }],
        settled_at_ms: record.completed_at_ms,
    };
    assert_eq!(
        ledger.record(&record, &settlement).unwrap(),
        LedgerAppend::Recorded
    );
    let read_entry = ledger.page(None, 1).unwrap().entries.remove(0);
    assert_eq!(read_entry.outcome.model_sequence.len(), 1);
    assert_eq!(read_entry.settlement.model_spend_micro, Some(big));
    let m = OutcomeLedgerMetrics::compute(std::slice::from_ref(&read_entry));
    assert_eq!(m.total_verified_spend_micro, big);
    // ceil div of (2^53+1)/1 stays exact; no float ever participates.
    assert_eq!(m.successful_run_cost_micro, big);
    // Two verified records with an odd micro sum: the per-change average
    // rounds UP (never understates spend).
    let mut second = record.clone();
    second.run_id = "run-big-2".into();
    let mut second_settlement = settlement.clone();
    second_settlement.run_id = second.run_id.clone();
    assert_eq!(
        ledger.record(&second, &second_settlement).unwrap(),
        LedgerAppend::Recorded
    );
    let report = ledger.metrics().unwrap();
    assert_eq!(report.metrics.runs, 2);
    assert_eq!(report.metrics.verified_changes, 2);
    assert_eq!(report.metrics.total_verified_spend_micro, big * 2);
    assert_eq!(report.metrics.successful_run_cost_micro, big);
}

#[test]
fn router_consumes_the_ledger_and_replay_never_double_counts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store.clone());
    let record = full_record(); // run-2026-0001: one failure + one success sample
    let settlement = complete_settlement(
        &record,
        &[0, 3_000_000],
        3_000_000,
        MeasuredMoney::Unavailable,
    );
    assert_eq!(
        ledger.record(&record, &settlement).unwrap(),
        LedgerAppend::Recorded
    );
    let view = StoreOutcomeStore::new(store.clone());
    let key = faktor_router::OutcomeKey {
        provider: "ollama".into(),
        model: "qwen3.8".into(),
        phase: RouterPhase::Implement,
        task_class: TaskClass::Hard,
        risk_bucket: RiskBucket::High,
    };
    let failure = view.stats(&key).expect("failure sample recorded");
    assert_eq!((failure.sample_count, failure.failures_first_pass), (1, 1));
    assert_eq!(failure.rework_cost_micro_sum, 3_000_000);
    assert_eq!(failure.rework_turns_sum, 2);
    let success_key = faktor_router::OutcomeKey {
        provider: "anthropic".into(),
        model: "claude-opus".into(),
        ..key.clone()
    };
    let success = view.stats(&success_key).expect("success sample recorded");
    assert_eq!((success.sample_count, success.successes_first_pass), (1, 1));
    // Replaying the identical record must NOT feed the registry twice.
    assert_eq!(
        ledger.record(&record, &settlement).unwrap(),
        LedgerAppend::Replayed
    );
    let again = view.stats(&key).unwrap();
    assert_eq!(again.sample_count, 1);
    assert_eq!(again.rework_cost_micro_sum, 3_000_000);
    // A conflicting re-append under the same run id is refused AND
    // leaves the registry untouched.
    let mut hostile = record.clone();
    hostile.wall_time_ms = 1;
    let err = ledger.record(&hostile, &settlement).unwrap_err();
    assert!(matches!(err, OutcomeLedgerError::Store(_)), "got {err:?}");
    assert_eq!(view.stats(&key).unwrap().sample_count, 1);
}

#[test]
fn invalid_records_are_refused_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store);
    let assert_invalid = |record: &TerminalEngineeringOutcome, settlement: &EconomicSettlement| {
        assert!(
            matches!(
                ledger.record(record, settlement),
                Err(OutcomeLedgerError::Invalid(_))
            ),
            "record must be refused before any write: {record:?}"
        );
    };
    let base = full_record();
    let base_settlement = complete_settlement(
        &base,
        &[0, 3_000_000],
        3_000_000,
        MeasuredMoney::Unavailable,
    );
    let mut r = base.clone();
    r.schema_version = 3;
    assert_invalid(&r, &base_settlement);
    let mut r = base.clone();
    r.run_id = "run-ünicode".into();
    let mut s = base_settlement.clone();
    s.run_id = r.run_id.clone();
    assert_invalid(&r, &s);
    let mut r = base.clone();
    r.proof.checks_passed = 6;
    assert_invalid(&r, &base_settlement);
    let mut r = base.clone();
    r.verified_first_verification_pass = true; // needs a deterministic verified run
    r.final_basis = FinalVerificationBasis::Unverified;
    assert_invalid(&r, &{
        let mut s = base_settlement.clone();
        s.rework_spend_micro = Some(0);
        s
    });
    let mut r = base.clone();
    r.verified_samples = vec![sample("p", "m", RouterPhase::Implement, true, 5, 0)];
    assert_invalid(&r, &base_settlement);
    // A known execution class disagreeing with the local bit is refused.
    let mut r = base.clone();
    r.model_sequence = vec![RunModelStep {
        provider: "p".into(),
        model: "m".into(),
        local: true,
        execution_class: ExecutionClass::HostedEconomy,
    }];
    assert_invalid(&r, &base_settlement);
    let mut r = base.clone();
    r.model_sequence = (0..=MAX_RUN_MODEL_STEPS)
        .map(|i| step("p", &format!("m{i}"), false))
        .collect();
    assert_invalid(&r, &base_settlement);
    let mut r = base.clone();
    r.final_snapshot = "x".repeat(MAX_SNAPSHOT_BYTES + 1);
    assert_invalid(&r, &base_settlement);
    // A pending settlement carrying amounts (a fabricated zero) is
    // refused by the model-level validation.
    let mut s = pending_settlement(&base.run_id);
    s.model_spend_micro = Some(0);
    assert_invalid(&base, &s);
    // A complete settlement whose step sum disagrees is refused.
    let mut s = base_settlement.clone();
    s.model_spend_micro = Some(999);
    assert_invalid(&base, &s);
    // A settlement whose run id disagrees with the outcome is refused.
    let mut s = base_settlement.clone();
    s.run_id = "other".into();
    assert_invalid(&base, &s);
    assert_eq!(ledger.count().unwrap(), 0, "no invalid record ever landed");
}

#[test]
fn page_cursor_is_total_and_metrics_fold_reports_honest_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store);
    for i in 0..5i64 {
        let mut r = full_record();
        r.run_id = format!("run-{i}");
        r.completed_at_ms = 1_000 + i;
        let s = complete_settlement(&r, &[0, 3_000_000], 3_000_000, MeasuredMoney::Unavailable);
        assert_eq!(ledger.record(&r, &s).unwrap(), LedgerAppend::Recorded);
    }
    let first = ledger.page(None, 2).unwrap();
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.entries[0].outcome.run_id, "run-4");
    let next = first.next.expect("full page carries its cursor");
    let second = ledger.page(Some(&next), 2).unwrap();
    assert_eq!(
        second
            .entries
            .iter()
            .map(|e| e.outcome.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run-2", "run-1"]
    );
    let third = ledger.page(second.next.as_ref(), 2).unwrap();
    assert_eq!(third.entries.len(), 1);
    assert_eq!(third.entries[0].outcome.run_id, "run-0");
    assert!(third.next.is_none(), "a short page ends the scan");
    let report = ledger.metrics().unwrap();
    assert!(!report.truncated);
    assert_eq!((report.scanned, report.metrics.runs), (5, 5));
}

#[test]
fn zero_and_hostile_metric_inputs_never_panic_or_fabricate() {
    assert_eq!(
        OutcomeLedgerMetrics::compute(&[]),
        OutcomeLedgerMetrics::default()
    );
    let mut hostile = full_record();
    hostile.proof = ProofCoverage {
        checks_passed: u64::MAX,
        checks_required: 1,
    };
    hostile.verified_samples.clear();
    let settlement = EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: hostile.run_id.clone(),
        seq: 0,
        money_state: MoneyState::Complete,
        reason: "hostile".into(),
        model_spend_micro: Some(u64::MAX),
        verification_spend_micro: Some(u64::MAX),
        rework_spend_micro: Some(u64::MAX),
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: vec![SettlementStep {
            provider: "p".into(),
            model: "m".into(),
            local: false,
            execution_class: ExecutionClass::HostedEconomy,
            spend_micro: u64::MAX,
        }],
        settled_at_ms: 0,
    };
    let entry = OutcomeLedgerEntry {
        outcome: hostile,
        settlement,
    };
    let m = OutcomeLedgerMetrics::compute(std::slice::from_ref(&entry));
    assert!(m.proof_coverage_ppm <= 1_000_000);
    assert!(m.rework_cost_ratio_ppm <= 1_000_000);
    assert!(m.successful_run_cost_micro > 0);
}

/// A fresh manager with one Running task (reservations are legal there)
/// and its handle.
async fn interleaving_env(
    root: &str,
) -> (
    Arc<faktor_session::SessionManager>,
    faktor_session::SessionHandle,
    faktor_core::id::TaskId,
    tempfile::TempDir,
) {
    use faktor_core::state::{TaskState, TaskTransition};
    use faktor_session::{SessionManager, Task as SessionTask, TaskBudget};

    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let workspace = manager.create_workspace(root).unwrap();
    let handle = manager
        .create_session(workspace, "ledger reads", "p", "m")
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(SessionTask {
            task_id,
            session_id: handle.id(),
            goal: "read-order interleaving".into(),
            acceptance_criteria: Vec::new(),
            plan: Vec::new(),
            attachments: Vec::new(),
            contract: Default::default(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, revision, TaskTransition::StartRunning, None)
        .unwrap();
    (manager, handle, task_id, dir)
}

/// Finding: the settled-spend read once PRECEDED the reservation read, so
/// a settle landing between them produced a `complete` row permanently
/// missing the reservation (never re-finalized). The fixed order reads
/// reservations FIRST; the probe settles the reservation exactly between
/// the two money reads, and the classification must stay `pending` —
/// never a complete projection without the money.
#[tokio::test]
async fn adversarial_settle_between_the_two_money_reads_never_completes_missing_money() {
    use faktor_core::state::TaskTransition;
    use faktor_session::DurableBudgetLedger;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (manager, handle, task_id, _dir) = interleaving_env("/tmp/ledger-interleave").await;
    let budget = DurableBudgetLedger::new(manager.clone());
    let identity = faktor_core::op::ModelCallAttempt::new(
        faktor_core::id::OpId::new(1),
        faktor_core::id::OpId::new(2),
        0,
    )
    .unwrap();
    let reservation = budget
        .reserve_attempt(
            handle.id(),
            task_id,
            identity,
            123_456,
            None,
            ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    // A's reservation read runs FIRST (the fixed order); this probe
    // settles the reservation exactly between A's two money reads.
    let fired = Arc::new(AtomicBool::new(false));
    let hook_store = manager.store();
    let hook_fired = fired.clone();
    let ledger = OutcomeLedger::with_after_reservation_read(
        manager.store(),
        Arc::new(move || {
            if hook_fired.swap(true, Ordering::SeqCst) {
                return;
            }
            hook_store
                .cost_settle(reservation.raw(), 123_456, None, Some(123_456), None, 999)
                .unwrap();
        }),
    );
    // A terminal task row so the record can land.
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, revision, TaskTransition::FailFromRunning, None)
        .unwrap();
    let run_id = session_terminal_run_id(handle.id());
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        Some(LedgerAppend::Recorded)
    );
    assert!(
        fired.load(Ordering::SeqCst),
        "the probe must have settled the reservation between the two reads"
    );
    // The classification consumed the PRE-settle reservation state
    // (open): `pending`, never complete-missing-money.
    let initial = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(initial.money_state, MoneyState::Pending);
    assert_eq!(initial.model_spend_micro, None);
    // The next finalization re-reads the now-settled reservations and
    // moves to complete WITH the measured amount.
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, &run_id)
            .unwrap(),
        SettlementFinalize::Appended
    );
    let settled = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(settled.money_state, MoneyState::Complete);
    assert_eq!(settled.model_spend_micro, Some(123_456));
    assert_eq!(settled.seq, 1);
}

/// Finding: finalization used to hard-fail forever when the stored
/// non-final state's reason drifted (store forbids pending -> pending /
/// unavailable -> unavailable), when a read recovered
/// (unavailable -> pending), and `complete` could never be corrected.
/// It must now be a deterministic no-op for equal states, append the
/// legal improvement, and append a SEMANTICALLY DIFFERENT complete as
/// the documented correction.
#[tokio::test]
async fn finalize_never_wedges_same_state_noops_and_complete_corrections_land() {
    use faktor_session::BudgetAuthority as _;
    use faktor_session::DurableBudgetLedger;

    let (manager, handle, task_id, _dir) = interleaving_env("/tmp/ledger-finalize").await;
    let budget = DurableBudgetLedger::new(manager.clone());
    let identity = faktor_core::op::ModelCallAttempt::new(
        faktor_core::id::OpId::new(1),
        faktor_core::id::OpId::new(2),
        0,
    )
    .unwrap();
    let reservation = budget
        .reserve_attempt(
            handle.id(),
            task_id,
            identity,
            123_456,
            None,
            ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let ledger = OutcomeLedger::new(manager.store());

    // (1) pending -> pending with a changed reason: the stored pending
    // reason differs from the derived one. The store forbids the
    // transition, so finalize must return the deterministic no-op —
    // never an error that would wedge every later settlement.
    let mut record = full_record();
    record.run_id = "run-pending".into();
    record.verified_samples.clear();
    let initial_pending = pending_settlement("run-pending");
    assert_eq!(
        ledger.record(&record, &initial_pending).unwrap(),
        LedgerAppend::Recorded
    );
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, "run-pending")
            .unwrap(),
        SettlementFinalize::Unchanged
    );
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, "run-pending")
            .unwrap(),
        SettlementFinalize::Unchanged,
        "a re-derivation at a new clock must stay a no-op, never a conflict"
    );
    let still = ledger.settlement("run-pending").unwrap().unwrap();
    assert_eq!((still.money_state, still.seq), (MoneyState::Pending, 0));

    // (2) unavailable -> pending: a recovered read (the ledger is
    // readable now and the reservation is open) is a legal improvement.
    let mut record = full_record();
    record.run_id = "run-recover".into();
    record.verified_samples.clear();
    let initial_unavailable = EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: "run-recover".into(),
        seq: 0,
        money_state: MoneyState::Unavailable,
        reason: "initial read failed".into(),
        model_spend_micro: None,
        verification_spend_micro: None,
        rework_spend_micro: None,
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: Vec::new(),
        settled_at_ms: 1_760_000_000_000,
    };
    assert_eq!(
        ledger.record(&record, &initial_unavailable).unwrap(),
        LedgerAppend::Recorded
    );
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, "run-recover")
            .unwrap(),
        SettlementFinalize::Appended
    );
    let recovered = ledger.settlement("run-recover").unwrap().unwrap();
    assert_eq!(recovered.money_state, MoneyState::Pending);
    assert_eq!(recovered.seq, 1);
    // An identical re-derivation of the same non-final state no-ops.
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, "run-recover")
            .unwrap(),
        SettlementFinalize::Unchanged
    );

    // B settles the reservation; the projection moves to complete.
    budget
        .settle_usage(handle.id(), reservation, 10, 0, 0, 5, Some(123_456), None)
        .await
        .unwrap();
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, "run-recover")
            .unwrap(),
        SettlementFinalize::Appended
    );
    let complete = ledger.settlement("run-recover").unwrap().unwrap();
    assert_eq!(complete.money_state, MoneyState::Complete);
    assert_eq!(complete.seq, 2);
    assert_eq!(complete.model_spend_micro, Some(123_456));

    // (3) complete -> complete correction: a run recorded `complete`
    // with a STALE projection; a later derivation whose money MOVED
    // appends the documented correction instead of being impossible.
    let mut record = full_record();
    record.run_id = "run-correct".into();
    record.verified_samples.clear();
    let stale_complete = EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: "run-correct".into(),
        seq: 0,
        money_state: MoneyState::Complete,
        reason: "stale complete".into(),
        model_spend_micro: Some(0),
        verification_spend_micro: Some(0),
        rework_spend_micro: Some(0),
        avoided_hosted: MeasuredMoney::Unavailable,
        steps: Vec::new(),
        settled_at_ms: 1_760_000_000_000,
    };
    assert_eq!(
        ledger.record(&record, &stale_complete).unwrap(),
        LedgerAppend::Recorded
    );
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, "run-correct")
            .unwrap(),
        SettlementFinalize::Appended
    );
    let corrected = ledger.settlement("run-correct").unwrap().unwrap();
    assert_eq!(corrected.seq, 1);
    assert_eq!(corrected.model_spend_micro, Some(123_456));
    assert_eq!(
        corrected.reason,
        "every reservation of the run is settled and measured"
    );
    // The identical re-derivation is a no-op once again.
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, "run-correct")
            .unwrap(),
        SettlementFinalize::AlreadySettled
    );
    assert_eq!(ledger.settlement("run-correct").unwrap().unwrap().seq, 1);
}

/// Finding: a page of up to 500 runs passed ALL ids to the store's
/// 400-bound batch settlement read, so a 401-run ledger failed with
/// Oversized (server 500 / doctor FAILED). The paginated lookup must
/// serve a full page.
#[test]
fn page_batches_settlement_lookups_beyond_the_store_batch_bound() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store);
    let total = faktor_store::MAX_ECONOMIC_SETTLEMENT_BATCH + 1;
    for i in 0..total {
        let mut record = full_record();
        record.run_id = format!("run-{i:04}");
        record.completed_at_ms = 1_000 + i as i64;
        record.verified_samples.clear();
        let settlement = complete_settlement(
            &record,
            &[0, 3_000_000],
            3_000_000,
            MeasuredMoney::Unavailable,
        );
        assert_eq!(
            ledger.record(&record, &settlement).unwrap(),
            LedgerAppend::Recorded
        );
    }
    let page = ledger.page(None, MAX_LEDGER_PAGE).unwrap();
    assert_eq!(
        page.entries.len(),
        total,
        "a full page exceeds the store's settlement batch bound"
    );
    assert!(page.next.is_none());
    assert!(page
        .entries
        .iter()
        .all(|entry| entry.settlement.money_state == MoneyState::Complete));
    let report = ledger.metrics().unwrap();
    assert_eq!(report.metrics.runs, total as u64);
    assert!(!report.truncated);
}

/// Finding: `page` silently dropped a store-valid but agent-invalid
/// settlement row (extra keys in the step projection) and fabricated an
/// `unavailable` projection, while the single-run path propagated the
/// typed corruption. Both paths must propagate consistently.
#[test]
fn page_propagates_store_valid_agent_invalid_settlement_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ledger = OutcomeLedger::new(store.clone());
    let record = full_record();
    let settlement = complete_settlement(
        &record,
        &[0, 3_000_000],
        3_000_000,
        MeasuredMoney::Unavailable,
    );
    assert_eq!(
        ledger.record(&record, &settlement).unwrap(),
        LedgerAppend::Recorded
    );
    // A store-valid (documented keys present, sum consistent) but
    // agent-invalid correction: the step carries an extra key the
    // agent's strict decode refuses.
    let mut hostile = settlement.to_row().unwrap();
    hostile.seq = 1;
    hostile.steps_json = serde_json::json!([
        {"provider": "ollama", "model": "qwen3.8", "local": true, "spend_micro": 0,
         "evil": 1},
        {"provider": "anthropic", "model": "claude-opus", "local": false,
         "spend_micro": 3_000_000}
    ])
    .to_string();
    assert_eq!(
        store.economic_settlement_append(&hostile).unwrap(),
        faktor_store::EconomicSettlementAppend::Appended(1)
    );
    let page_err = ledger.page(None, 10).unwrap_err();
    assert!(
        matches!(page_err, OutcomeLedgerError::Corrupt { .. }),
        "page must propagate the typed corruption, got {page_err:?}"
    );
    let single_err = ledger.settlement(&record.run_id).unwrap_err();
    assert!(
        matches!(single_err, OutcomeLedgerError::Corrupt { .. }),
        "the single-run read must propagate it identically, got {single_err:?}"
    );
}

/// The hosted-only denominator is PINNED: every run with a non-empty
/// model sequence stays in the denominator — including runs whose
/// classes are all `unknown` (no durable authority), which can never be
/// proven hosted-only and therefore never enter the numerator. A run
/// with NO steps is outside the denominator entirely.
#[test]
fn hosted_only_denominator_counts_every_run_with_steps_including_unknown_classes() {
    fn entry(run_id: &str, sequence: Vec<RunModelStep>, spend: u64) -> OutcomeLedgerEntry {
        let mut record = full_record();
        record.run_id = run_id.into();
        record.model_sequence = sequence;
        record.verified_samples.clear();
        let settlement = EconomicSettlement {
            schema_version: SETTLEMENT_SCHEMA_V,
            run_id: run_id.into(),
            seq: 0,
            money_state: MoneyState::Complete,
            reason: "test".into(),
            model_spend_micro: Some(spend),
            verification_spend_micro: Some(0),
            rework_spend_micro: Some(0),
            avoided_hosted: MeasuredMoney::Unavailable,
            steps: if spend == 0 {
                Vec::new()
            } else {
                vec![SettlementStep {
                    provider: "p".into(),
                    model: "m".into(),
                    local: false,
                    execution_class: ExecutionClass::Unknown,
                    spend_micro: spend,
                }]
            },
            settled_at_ms: 1_760_000_000_000,
        };
        OutcomeLedgerEntry {
            outcome: record,
            settlement,
        }
    }
    let entries = vec![
        entry(
            "hosted",
            vec![step_classed("hosted", "m", ExecutionClass::HostedEconomy)],
            100,
        ),
        entry(
            "unknown",
            vec![step_classed("hosted", "m", ExecutionClass::Unknown)],
            200,
        ),
    ];
    let m = OutcomeLedgerMetrics::compute(&entries);
    assert_eq!(
        m.hosted_only_runs, 1,
        "the unknown-class run is not proven hosted"
    );
    assert_eq!(
        m.hosted_only_ppm, 500_000,
        "the unknown-class run stays in the denominator (runs with steps)"
    );
    let with_empty = {
        let mut entries = entries.clone();
        entries.push(entry("empty", Vec::new(), 0));
        OutcomeLedgerMetrics::compute(&entries)
    };
    assert_eq!(
        with_empty.hosted_only_ppm, 500_000,
        "a run with no steps never enters the denominator"
    );
}
