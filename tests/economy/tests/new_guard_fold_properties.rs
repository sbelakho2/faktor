//! NEW-GUARD adversarial verification (measurement layer, owned by the
//! mutation campaign): model-based properties for the Engineering Outcome
//! Ledger's metrics fold — the economic truth the VCSC and the
//! avoided-hosted counterfactual rest on.
//!
//! Invariants pinned here (written from the SPEC, not from the fold):
//!
//! - MONETARY metrics fold ONLY `complete` settlements: a `pending` or
//!   `unavailable` row NEVER contributes to `total_spend_micro`,
//!   `total_verified_spend_micro`, rework, `money_complete_runs` or the VCSC
//!   numerator/denominator — even when a hostile caller hands the pure fold
//!   amounts on an incomplete row;
//! - as settlements land one-by-one the monetary counters move by EXACT
//!   deltas (monotone; each landing contributes exactly its own fact) and
//!   VCSC is exactly the ceiling of the eligible-cohort spend over the
//!   verified money-complete changes;
//! - the avoided-hosted counterfactual is `None` until at least one MEASURED
//!   counterfactual exists (a measured zero is a zero; absence is never
//!   fabricated into `0`);
//! - classification reads the durable execution class only: an `Unknown`
//!   step is counted as neither local nor hosted — including a legacy step
//!   whose old `local` bit claims locality.
//!
//! Mutation oracles (specs under `scripts/mutations/`):
//! - `INV-VCSC-MONEY-COMPLETE-COHORT` (incomplete rows folded into spend);
//! - `INV-AVOIDED-HOSTED-UNMEASURED-NULL` (unmeasured coerced to `Some(0)`).

use faktor_agent::{
    ChangedCodeScale, EconomicSettlement, FinalVerificationBasis, MeasuredMoney, MoneyState,
    OutcomeLedgerEntry, OutcomeLedgerMetrics, ProofCoverage, RunModelStep, SettlementStep,
    TerminalEngineeringOutcome, SETTLEMENT_SCHEMA_V,
};
use faktor_core::model::{ExecutionClass, RiskBucket, TaskClass};

/// Deterministic seeded LCG (the repository's platform-independent property
/// driver; numerical-recipe constants only).
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0) is not a draw");
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

// ---------------------------------------------------------------------------
// entry builders (the pure fold consumes in-memory entries)
// ---------------------------------------------------------------------------

fn outcome(
    run: &str,
    verified: bool,
    first_pass: bool,
    steps: Vec<RunModelStep>,
) -> TerminalEngineeringOutcome {
    TerminalEngineeringOutcome {
        schema_version: faktor_agent::OUTCOME_RECORD_SCHEMA_V,
        run_id: run.to_string(),
        session_id: 1,
        task_id: 1,
        task_class: TaskClass::Medium,
        risk_class: RiskBucket::Low,
        candidate_snapshot: None,
        final_snapshot: "0".repeat(64),
        accepted: verified,
        integrated: verified,
        verified_first_verification_pass: verified && first_pass,
        repair_attempts: 0,
        model_sequence: steps,
        wall_time_ms: 10,
        human_interventions: 0,
        proof: ProofCoverage {
            checks_passed: 1,
            checks_required: 1,
        },
        change_scale: ChangedCodeScale::default(),
        rollback_events: 0,
        recovery_events: 0,
        final_basis: if verified {
            FinalVerificationBasis::DeterministicChecks
        } else {
            FinalVerificationBasis::Unverified
        },
        frontier_escalations: 0,
        completed_at_ms: 1_000,
        verified_samples: vec![],
    }
}

fn settlement(
    run: &str,
    state: MoneyState,
    model: Option<u64>,
    verification: Option<u64>,
    rework: Option<u64>,
    avoided: MeasuredMoney,
) -> EconomicSettlement {
    EconomicSettlement {
        schema_version: SETTLEMENT_SCHEMA_V,
        run_id: run.to_string(),
        seq: 0,
        money_state: state,
        reason: "property".into(),
        model_spend_micro: model,
        verification_spend_micro: verification,
        rework_spend_micro: rework,
        avoided_hosted: avoided,
        steps: Vec::<SettlementStep>::new(),
        settled_at_ms: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn entry(
    run: &str,
    state: MoneyState,
    verified: bool,
    model: Option<u64>,
    verification: Option<u64>,
    rework: Option<u64>,
    avoided: MeasuredMoney,
    steps: Vec<RunModelStep>,
) -> OutcomeLedgerEntry {
    OutcomeLedgerEntry {
        outcome: outcome(run, verified, verified, steps),
        settlement: settlement(run, state, model, verification, rework, avoided),
    }
}

fn ceil_div(n: u128, d: u128) -> u64 {
    if d == 0 {
        0
    } else {
        u64::try_from(n.div_ceil(d)).unwrap_or(u64::MAX)
    }
}

// ---------------------------------------------------------------------------
// VCSC: incomplete rows never participate, even with hostile amounts
// ---------------------------------------------------------------------------

/// A hostile in-memory cohort: the pending/unavailable entries carry huge
/// fabricated amounts. The monetary metrics must be EXACTLY those of the
/// complete cohort, and zeroing the incomplete amounts must not move any
/// monetary metric by a single micro.
#[test]
fn vcsc_excludes_incomplete_rows_even_with_hostile_amounts() {
    let complete = entry(
        "run-complete",
        MoneyState::Complete,
        true,
        Some(1_000_000),
        Some(0),
        Some(0),
        MeasuredMoney::Unavailable,
        vec![],
    );
    let hostile_pending = entry(
        "run-pending",
        MoneyState::Pending,
        true,
        Some(9_999_999),
        Some(7_777_777),
        Some(6_666_666),
        MeasuredMoney::Unavailable,
        vec![],
    );
    let hostile_unavailable = entry(
        "run-unavailable",
        MoneyState::Unavailable,
        true,
        Some(8_888_888),
        Some(5_555_555),
        Some(0),
        MeasuredMoney::Unavailable,
        vec![],
    );
    let hostile = OutcomeLedgerMetrics::compute(&[
        complete.clone(),
        hostile_pending.clone(),
        hostile_unavailable.clone(),
    ]);

    assert_eq!(hostile.money_complete_runs, 1);
    assert_eq!(hostile.money_pending_runs, 1);
    assert_eq!(hostile.money_unavailable_runs, 1);
    assert_eq!(hostile.money_incomplete_excluded, 2);
    assert_eq!(
        hostile.total_spend_micro, 1_000_000,
        "an incomplete row's fabricated spend leaked into the eligible cohort"
    );
    assert_eq!(hostile.total_verified_spend_micro, 1_000_000);
    assert_eq!(hostile.verified_changes_money_complete, 1);
    assert_eq!(
        hostile.verified_cost_per_successful_change_micro, 1_000_000,
        "VCSC must divide the complete-cohort spend only"
    );
    assert_eq!(hostile.successful_run_cost_micro, 1_000_000);

    // Zeroing the incomplete amounts changes NOTHING monetary: the rows are
    // excluded by state, never by their (hostile) value.
    let zeroed_pending = entry(
        "run-pending",
        MoneyState::Pending,
        true,
        Some(0),
        Some(0),
        Some(0),
        MeasuredMoney::Unavailable,
        vec![],
    );
    let zeroed_unavailable = entry(
        "run-unavailable",
        MoneyState::Unavailable,
        true,
        Some(0),
        Some(0),
        Some(0),
        MeasuredMoney::Unavailable,
        vec![],
    );
    let zeroed = OutcomeLedgerMetrics::compute(&[complete, zeroed_pending, zeroed_unavailable]);
    assert_eq!(hostile.total_spend_micro, zeroed.total_spend_micro);
    assert_eq!(
        hostile.total_verified_spend_micro,
        zeroed.total_verified_spend_micro
    );
    assert_eq!(
        hostile.total_rework_spend_micro,
        zeroed.total_rework_spend_micro
    );
    assert_eq!(
        hostile.verified_cost_per_successful_change_micro,
        zeroed.verified_cost_per_successful_change_micro
    );
}

/// As settlements land one-by-one, every monetary counter moves by exactly
/// the landed fact; incomplete landings move only the exclusion counts; VCSC
/// is the exact ceiling of eligible-cohort spend over verified complete
/// changes at every prefix.
#[test]
fn monetary_metrics_move_exactly_as_settlements_land() {
    let mut entries: Vec<OutcomeLedgerEntry> = Vec::new();
    for index in 0..60u64 {
        let mut lcg = Lcg::new(0xC0FF_EE00_0000_0001 ^ index.wrapping_mul(0x9E37_79B9));
        let state = match lcg.below(5) {
            0 => MoneyState::Pending,
            1 => MoneyState::Unavailable,
            _ => MoneyState::Complete,
        };
        let verified = lcg.chance(50);
        let (model, verification, rework) = if state == MoneyState::Complete {
            let model = lcg.below(2_000_000);
            let verification = lcg.below(100_000);
            let rework = lcg.below(model + 1);
            (Some(model), Some(verification), Some(rework))
        } else {
            (None, None, None)
        };
        entries.push(entry(
            &format!("run-{index}"),
            state,
            verified,
            model,
            verification,
            rework,
            MeasuredMoney::Unavailable,
            vec![],
        ));
    }

    let mut expected_total: u128 = 0;
    let mut expected_verified_total: u128 = 0;
    let mut expected_rework: u128 = 0;
    let mut expected_complete = 0u128;
    let mut expected_verified_complete = 0u128;
    for (index, entry) in entries.iter().enumerate() {
        let before = OutcomeLedgerMetrics::compute(&entries[..index]);
        let after = OutcomeLedgerMetrics::compute(&entries[..=index]);
        let landed = &entry.settlement;
        let complete = landed.money_state == MoneyState::Complete;
        if complete {
            let spend = u128::from(landed.model_spend_micro.unwrap_or(0))
                + u128::from(landed.verification_spend_micro.unwrap_or(0));
            expected_total += spend;
            expected_rework += u128::from(landed.rework_spend_micro.unwrap_or(0));
            expected_complete += 1;
            if entry.outcome.verified() {
                expected_verified_total += spend;
                expected_verified_complete += 1;
            }
        }

        assert!(
            after.money_complete_runs >= before.money_complete_runs,
            "money_complete_runs must be monotone as settlements land"
        );
        assert!(
            after.total_spend_micro >= before.total_spend_micro,
            "total_spend_micro must be monotone as settlements land"
        );
        if complete {
            assert_eq!(after.money_complete_runs, before.money_complete_runs + 1);
            assert_eq!(
                after.money_incomplete_excluded, before.money_incomplete_excluded,
                "a complete landing must not change the exclusion count"
            );
        } else {
            assert_eq!(
                after.money_complete_runs, before.money_complete_runs,
                "an incomplete landing must never enter the monetary cohort"
            );
            assert_eq!(
                after.money_incomplete_excluded,
                before.money_incomplete_excluded + 1
            );
        }
        assert_eq!(u128::from(after.total_spend_micro), expected_total);
        assert_eq!(
            u128::from(after.total_verified_spend_micro),
            expected_verified_total
        );
        assert_eq!(u128::from(after.total_rework_spend_micro), expected_rework);
        assert_eq!(u128::from(after.money_complete_runs), expected_complete);
        assert_eq!(
            u128::from(after.verified_changes_money_complete),
            expected_verified_complete
        );
        assert_eq!(
            after.verified_cost_per_successful_change_micro,
            ceil_div(expected_total, expected_verified_complete),
            "VCSC must be the exact ceiling of the eligible-cohort spend"
        );
    }
}

// ---------------------------------------------------------------------------
// avoided-hosted: unmeasured is NULL, a measured zero is a measured zero
// ---------------------------------------------------------------------------

/// `avoided_hosted_inference_micro` is `None` while no MEASURED counterfactual
/// exists and `Some(exact sum)` (with the exact measured-run count) once one
/// does — a measured zero stays a measured zero, never an absence; absence
/// never becomes a fabricated zero.
#[test]
fn unmeasured_avoided_hosted_is_null_never_a_fabricated_zero() {
    let empty = OutcomeLedgerMetrics::compute(&[]);
    assert_eq!(
        empty.avoided_hosted_inference_micro, None,
        "an unmeasured counterfactual must never be coerced into a zero"
    );
    assert_eq!(empty.avoided_hosted_measured_runs, 0);

    let unmeasured: Vec<OutcomeLedgerEntry> = (0..5)
        .map(|index| {
            entry(
                &format!("u-{index}"),
                MoneyState::Complete,
                true,
                Some(1),
                Some(0),
                Some(0),
                MeasuredMoney::Unavailable,
                vec![],
            )
        })
        .collect();
    let unmeasured = OutcomeLedgerMetrics::compute(&unmeasured);
    assert_eq!(
        unmeasured.avoided_hosted_inference_micro, None,
        "no measured counterfactual may NOT be coerced into a zero"
    );
    assert_eq!(unmeasured.avoided_hosted_measured_runs, 0);

    let measured_zero = OutcomeLedgerMetrics::compute(&[entry(
        "z-0",
        MoneyState::Complete,
        true,
        Some(1),
        Some(0),
        Some(0),
        MeasuredMoney::Measured(0),
        vec![],
    )]);
    assert_eq!(
        measured_zero.avoided_hosted_inference_micro,
        Some(0),
        "a measured zero is a MEASURED zero, not an absence"
    );
    assert_eq!(measured_zero.avoided_hosted_measured_runs, 1);

    let mixed = OutcomeLedgerMetrics::compute(&[
        entry(
            "m-0",
            MoneyState::Complete,
            true,
            Some(1),
            Some(0),
            Some(0),
            MeasuredMoney::Measured(11),
            vec![],
        ),
        entry(
            "m-1",
            MoneyState::Complete,
            false,
            Some(1),
            Some(0),
            Some(0),
            MeasuredMoney::Unavailable,
            vec![],
        ),
        entry(
            "m-2",
            MoneyState::Complete,
            true,
            Some(1),
            Some(0),
            Some(0),
            MeasuredMoney::Measured(0),
            vec![],
        ),
        entry(
            "m-3",
            MoneyState::Complete,
            true,
            Some(1),
            Some(0),
            Some(0),
            MeasuredMoney::Measured(4),
            vec![],
        ),
    ]);
    assert_eq!(mixed.avoided_hosted_inference_micro, Some(15));
    assert_eq!(mixed.avoided_hosted_measured_runs, 3);

    // Additivity of disjoint cohorts: the union's counterfactual is exactly
    // the sum of the measured parts (or None when neither part measured).
    let part_a = vec![entry(
        "a-0",
        MoneyState::Complete,
        true,
        Some(1),
        Some(0),
        Some(0),
        MeasuredMoney::Measured(3),
        vec![],
    )];
    let part_b = vec![
        entry(
            "b-0",
            MoneyState::Complete,
            true,
            Some(1),
            Some(0),
            Some(0),
            MeasuredMoney::Measured(0),
            vec![],
        ),
        entry(
            "b-1",
            MoneyState::Pending,
            false,
            None,
            None,
            None,
            MeasuredMoney::Unavailable,
            vec![],
        ),
    ];
    let a = OutcomeLedgerMetrics::compute(&part_a);
    let b = OutcomeLedgerMetrics::compute(&part_b);
    let mut union = part_a;
    union.extend(part_b);
    let union = OutcomeLedgerMetrics::compute(&union);
    assert_eq!(
        union.avoided_hosted_inference_micro,
        Some(a.avoided_hosted_inference_micro.unwrap() + b.avoided_hosted_inference_micro.unwrap())
    );
    assert_eq!(
        union.avoided_hosted_measured_runs,
        a.avoided_hosted_measured_runs + b.avoided_hosted_measured_runs
    );
}

// ---------------------------------------------------------------------------
// classification: Unknown is counted as neither local nor hosted
// ---------------------------------------------------------------------------

fn step(class: ExecutionClass, legacy_local: bool) -> RunModelStep {
    RunModelStep {
        provider: "provider".into(),
        model: "model".into(),
        local: legacy_local,
        execution_class: class,
    }
}

/// Exhaustive over every model sequence of length 1..=2 drawn from
/// {Local, HostedEconomy, Frontier, Unknown}: every local/hosted metric
/// matches the SPEC reference exactly, and an Unknown step never lands in a
/// local or hosted count.
#[test]
fn classification_unknown_steps_are_counted_as_neither_local_nor_hosted() {
    let alphabet = [
        ExecutionClass::Local,
        ExecutionClass::HostedEconomy,
        ExecutionClass::Frontier,
        ExecutionClass::Unknown,
    ];
    let mut sequences: Vec<Vec<ExecutionClass>> = Vec::new();
    for first in alphabet {
        sequences.push(vec![first]);
        for second in alphabet {
            sequences.push(vec![first, second]);
        }
    }
    assert_eq!(
        sequences.len(),
        20,
        "the exhaustive corpus must cover every sequence"
    );

    for (index, classes) in sequences.iter().enumerate() {
        let steps: Vec<RunModelStep> = classes
            .iter()
            .map(|class| step(*class, class.is_local()))
            .collect();
        let verified = true;
        let metrics = OutcomeLedgerMetrics::compute(&[entry(
            &format!("c-{index}"),
            MoneyState::Complete,
            verified,
            Some(0),
            Some(0),
            Some(0),
            MeasuredMoney::Unavailable,
            steps,
        )]);

        let any_local = classes.iter().any(|class| class.is_local());
        let all_local = classes.iter().all(|class| class.is_local());
        let last_local = classes.last().is_some_and(|class| class.is_local());
        let last_hosted = classes.last().is_some_and(|class| class.is_hosted());
        let first_local = classes.first().is_some_and(|class| class.is_local());
        let all_hosted = classes.iter().all(|class| class.is_hosted());

        assert_eq!(metrics.local_runs, u64::from(any_local), "{classes:?}");
        assert_eq!(metrics.local_only_runs, u64::from(all_local), "{classes:?}");
        assert_eq!(
            metrics.local_only_verified_changes,
            u64::from(all_local && verified),
            "{classes:?}"
        );
        assert_eq!(
            metrics.local_final_runs,
            u64::from(last_local),
            "{classes:?}"
        );
        assert_eq!(
            metrics.local_final_verified_changes,
            u64::from(last_local && verified),
            "{classes:?}"
        );
        assert_eq!(
            metrics.local_first_runs,
            u64::from(first_local),
            "{classes:?}"
        );
        assert_eq!(
            metrics.local_to_hosted_fallback_runs,
            u64::from(first_local && last_hosted),
            "{classes:?}"
        );
        assert_eq!(
            metrics.local_first_success_changes,
            u64::from(first_local && !last_hosted && last_local && verified),
            "{classes:?}"
        );
        assert_eq!(
            metrics.hosted_only_runs,
            u64::from(all_hosted),
            "{classes:?}"
        );
    }
}

/// Deterministic witness for the unknown-never-counted rule including the
/// legacy `local` bit: a pre-class payload whose old bit claims locality must
/// still be counted as neither local nor hosted (the durable class decides),
/// and a fallback run (local -> known hosted) is never a local success.
#[test]
fn legacy_local_bit_on_an_unknown_step_is_never_a_local_count() {
    let metrics = OutcomeLedgerMetrics::compute(&[entry(
        "legacy",
        MoneyState::Complete,
        true,
        Some(0),
        Some(0),
        Some(0),
        MeasuredMoney::Unavailable,
        vec![step(ExecutionClass::Unknown, true)],
    )]);
    assert_eq!(metrics.local_runs, 0, "Unknown must not be counted local");
    assert_eq!(metrics.local_only_runs, 0);
    assert_eq!(metrics.local_final_runs, 0);
    assert_eq!(metrics.local_first_runs, 0);
    assert_eq!(
        metrics.hosted_only_runs, 0,
        "Unknown must not be counted hosted"
    );

    let fallback = OutcomeLedgerMetrics::compute(&[entry(
        "fallback",
        MoneyState::Complete,
        true,
        Some(0),
        Some(0),
        Some(0),
        MeasuredMoney::Unavailable,
        vec![
            step(ExecutionClass::Local, true),
            step(ExecutionClass::HostedEconomy, false),
        ],
    )]);
    assert_eq!(fallback.local_to_hosted_fallback_runs, 1);
    assert_eq!(
        fallback.local_first_success_changes, 0,
        "a local-to-hosted fallback is never a local success"
    );
}
