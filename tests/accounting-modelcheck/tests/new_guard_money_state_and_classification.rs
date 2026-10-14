//! NEW-GUARD adversarial verification (economic/measurement/classification
//! layers, owned by the mutation campaign): model-based property tests plus
//! deterministic planted-violation witnesses for two guards that the
//! standard suite does not pin:
//!
//! 1. the durable settlement lifecycle's transition legality
//!    (`crates/store/src/outcome_ledger.rs`): only
//!    `pending -> complete|unavailable`, `unavailable -> complete|pending`
//!    (a recovered read) and `complete -> complete` corrections are legal;
//!    once the money is `complete` the projection is TERMINAL — no later
//!    `pending` or `unavailable` event may ever land, and an illegal append
//!    must leave the latest row byte-identical;
//! 2. the durable execution-classification authority
//!    (`crates/session/src/billing_read.rs`): a numeric zero is NEVER
//!    locality — only a route-time `PriceAuthority::LocalZero` (or a known
//!    persisted class) may classify a row local; an `Unknown` class is
//!    counted as neither local nor hosted.
//!
//! The reference model in this file is written from the SPEC, independently
//! of the production implementation, so a planted relaxation of either guard
//! makes the model disagree with the store and fails these gates.
//!
//! Mutation oracles (specs under `scripts/mutations/`):
//! - `INV-SETTLEMENT-MONEY-STATE-LEGALITY` (complete -> unavailable admitted);
//! - `INV-LOCALZERO-CLASSIFICATION-AUTHORITY` (cost == 0 => Local heuristic).

use faktor_core::model::{ExecutionClass, PriceAuthority};
use faktor_session::DurableSpendRow;
use faktor_store::{EconomicSettlementAppend, EconomicSettlementRow, Store, StoreError};
use faktor_tests_accounting_modelcheck::Lcg;

// ---------------------------------------------------------------------------
// 1. settlement lifecycle: reference model + property driver
// ---------------------------------------------------------------------------

const STATES: [&str; 3] = ["complete", "pending", "unavailable"];

/// The spec's legal transition relation, written independently of the
/// production `settlement_transition_legal`.
fn legal_transition(latest: &str, next: &str) -> bool {
    match latest {
        "complete" => next == "complete",
        "pending" => next == "complete" || next == "unavailable",
        "unavailable" => next == "complete" || next == "pending",
        _ => false,
    }
}

/// One entry of the reference model: the LATEST stored event's semantic key
/// plus its sequence position.
#[derive(Clone)]
struct ModelRow {
    state: &'static str,
    seq: i64,
    event: EconomicSettlementRow,
}

#[derive(Debug, PartialEq, Eq)]
enum Expect {
    Appended(i64),
    Duplicate,
    Conflict,
}

/// The complete semantic row identity the store's duplicate check uses
/// (`seq` and `settled_at_ms` excluded).
fn semantically_equal(a: &EconomicSettlementRow, b: &EconomicSettlementRow) -> bool {
    a.schema_version == b.schema_version
        && a.state == b.state
        && a.reason == b.reason
        && a.model_spend_micro == b.model_spend_micro
        && a.verification_spend_micro == b.verification_spend_micro
        && a.rework_spend_micro == b.rework_spend_micro
        && a.avoided_hosted_micro == b.avoided_hosted_micro
        && a.avoided_hosted_measured == b.avoided_hosted_measured
        && a.steps_json == b.steps_json
}

/// The model's expectation for one submitted event.
fn expect_append(latest: Option<&ModelRow>, next: &EconomicSettlementRow) -> Expect {
    match latest {
        None => Expect::Appended(0),
        Some(latest) => {
            if semantically_equal(&latest.event, next) {
                Expect::Duplicate
            } else if legal_transition(latest.state, &next.state) {
                Expect::Appended(latest.seq + 1)
            } else {
                Expect::Conflict
            }
        }
    }
}

/// A VALID store row of `state` (the store's own `validate` rules hold by
/// construction; `salt` keeps same-state events semantically distinct).
fn settlement(run_id: &str, state: &str, salt: u64) -> EconomicSettlementRow {
    let complete = state == "complete";
    let model = 100 + salt as i64;
    let steps = format!(r#"[{{"provider":"p","model":"m","local":false,"spend_micro":{model}}}]"#);
    EconomicSettlementRow {
        run_id: run_id.to_string(),
        seq: 0,
        schema_version: 1,
        state: state.to_string(),
        reason: format!("{state}#{salt}"),
        model_spend_micro: complete.then_some(model),
        verification_spend_micro: complete.then_some(0),
        rework_spend_micro: complete.then_some(0),
        avoided_hosted_micro: complete.then_some(7 + salt as i64),
        avoided_hosted_measured: complete,
        steps_json: if complete { steps } else { "[]".to_string() },
        settled_at_ms: 1_000 + salt as i64,
    }
}

/// Drive ONE submitted event against both the model and the durable store,
/// asserting agreement plus the terminal-immutability invariant.
fn drive(
    store: &Store,
    run_id: &str,
    latest: &mut Option<ModelRow>,
    submitted: &EconomicSettlementRow,
    context: &str,
) {
    let expected = expect_append(latest.as_ref(), submitted);
    let outcome = store.economic_settlement_append(submitted);
    match (&expected, outcome) {
        (Expect::Appended(seq), Ok(EconomicSettlementAppend::Appended(actual))) => {
            assert_eq!(
                &actual, seq,
                "{context}: the store must append at the model's next sequence"
            );
        }
        (Expect::Duplicate, Ok(EconomicSettlementAppend::Duplicate)) => {}
        (Expect::Conflict, Err(StoreError::Conflict(message))) => {
            assert!(
                message.contains(&submitted.state) || message.contains("may not move"),
                "{context}: the conflict must be the lifecycle refusal, got {message:?}"
            );
        }
        (expected, outcome) => panic!(
            "{context}: store disagrees with the spec model: expected {expected:?}, got \
             {outcome:?}"
        ),
    }

    match expected {
        Expect::Appended(seq) => {
            let previous = latest.as_ref().map(|row| row.state);
            if let Some(previous) = previous {
                assert!(
                    legal_transition(previous, &submitted.state),
                    "{context}: the store ACCEPTED an illegal transition {previous} -> {}",
                    submitted.state
                );
            }
            *latest = Some(ModelRow {
                state: if submitted.state == "complete" {
                    "complete"
                } else if submitted.state == "pending" {
                    "pending"
                } else {
                    "unavailable"
                },
                seq,
                event: submitted.clone(),
            });
        }
        Expect::Duplicate | Expect::Conflict => {}
    }

    // The durable latest row must equal the model's, always.
    let stored = store
        .economic_settlement_latest(run_id)
        .expect("latest read");
    match (&*latest, stored) {
        (None, None) => {}
        (Some(model), Some(row)) => {
            assert_eq!(
                row.seq, model.seq,
                "{context}: durable seq drifted from the model"
            );
            assert_eq!(
                row.state, model.state,
                "{context}: durable state drifted from the model"
            );
            assert!(
                semantically_equal(&row, &model.event),
                "{context}: durable latest row is not the model's event"
            );
        }
        (model, stored) => panic!(
            "{context}: durability mismatch: model {:?}, store {:?}",
            model.as_ref().map(|m| (m.state, m.seq)),
            stored.map(|r| (r.state, r.seq))
        ),
    }
}

/// TERMINAL immutability: once the latest projection is `complete`, no later
/// accepted event may change the state away from `complete`.
#[test]
fn terminal_money_state_is_immutable_through_the_durable_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let run = "run-terminal";
    let mut latest = None;
    // pending -> complete: the money lands.
    drive(
        &store,
        run,
        &mut latest,
        &settlement(run, "pending", 0),
        "initial pending",
    );
    let complete = settlement(run, "complete", 1);
    drive(&store, run, &mut latest, &complete, "pending -> complete");
    assert_eq!(latest.as_ref().unwrap().state, "complete");
    assert_eq!(latest.as_ref().unwrap().seq, 1);
    // A verbatim replay is a no-op; the seq never moves.
    drive(&store, run, &mut latest, &complete, "verbatim replay");
    assert_eq!(latest.as_ref().unwrap().seq, 1);
    // complete -> complete CORRECTION is still legal (the guard must not
    // over-block): a different fact appends exactly one event.
    let mut corrected = complete.clone();
    corrected.reason = "correction".into();
    corrected.rework_spend_micro = Some(5);
    corrected.avoided_hosted_micro = None;
    corrected.avoided_hosted_measured = false;
    drive(
        &store,
        run,
        &mut latest,
        &corrected,
        "complete -> complete correction",
    );
    assert_eq!(latest.as_ref().unwrap().seq, 2);
    // complete -> pending and complete -> unavailable are typed conflicts
    // and the terminal row is byte-identical afterwards.
    let before = store.economic_settlement_latest(run).unwrap().unwrap();
    for state in ["pending", "unavailable"] {
        let regress = settlement(run, state, 9);
        let err = store.economic_settlement_append(&regress).unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict(_)),
            "complete -> {state} must be refused typed, got {err:?}"
        );
        let after = store.economic_settlement_latest(run).unwrap().unwrap();
        assert_eq!(
            before, after,
            "a refused complete -> {state} append must leave the latest row untouched"
        );
    }
}

/// The exhaustive bounded corpus (every state sequence of length 1..=3) plus
/// seeded longer interleavings (including verbatim replays): every append
/// agrees with the SPEC model, only legal transitions are accepted and
/// terminal immutability holds at every step.
#[test]
fn settlement_lifecycle_only_legal_transitions_over_exhaustive_and_seeded_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();

    // Exhaustive: 3 + 9 + 27 = 39 sequences.
    let mut ordinal = 0usize;
    for length in 1..=3usize {
        let mut indices = vec![0usize; length];
        loop {
            let run = format!("run-exh-{ordinal}");
            let mut latest = None;
            for (position, index) in indices.iter().enumerate() {
                let state = STATES[*index];
                let event = settlement(&run, state, position as u64);
                drive(
                    &store,
                    &run,
                    &mut latest,
                    &event,
                    &format!("exhaustive {indices:?} step {position}"),
                );
                if let Some(row) = &latest {
                    if row.state == "complete" {
                        assert_eq!(
                            store
                                .economic_settlement_latest(&run)
                                .unwrap()
                                .unwrap()
                                .state,
                            "complete",
                            "terminal immutability: a complete projection must never regress"
                        );
                    }
                }
            }
            ordinal += 1;
            let mut cursor = length;
            loop {
                if cursor == 0 {
                    break;
                }
                cursor -= 1;
                indices[cursor] += 1;
                if indices[cursor] < STATES.len() {
                    break;
                }
                indices[cursor] = 0;
                if cursor == 0 {
                    break;
                }
            }
            if indices.iter().all(|index| *index == 0) {
                break;
            }
        }
    }
    assert_eq!(
        ordinal, 39,
        "the exhaustive corpus must cover every sequence"
    );

    // Seeded longer interleavings: fresh events plus verbatim replays of
    // earlier submitted rows (the store compares against the LATEST row only).
    for sequence in 0..120u64 {
        let mut lcg = Lcg::new(0x5E77_1E00_0000_0001 ^ sequence.wrapping_mul(0x9E37_79B9));
        let run = format!("run-seeded-{sequence}");
        let mut latest = None;
        let mut submitted: Vec<EconomicSettlementRow> = Vec::new();
        let steps = 2 + lcg.below(9);
        for position in 0..steps {
            let event = if position > 0 && lcg.chance(30) {
                submitted[lcg.below(submitted.len() as u64) as usize].clone()
            } else {
                settlement(
                    &run,
                    STATES[lcg.below(STATES.len() as u64) as usize],
                    10_000 * sequence + position,
                )
            };
            drive(
                &store,
                &run,
                &mut latest,
                &event,
                &format!("seeded sequence {sequence} step {position}"),
            );
            submitted.push(event);
            if latest.as_ref().is_some_and(|row| row.state == "complete") {
                assert_eq!(
                    store
                        .economic_settlement_latest(&run)
                        .unwrap()
                        .unwrap()
                        .state,
                    "complete",
                    "terminal immutability drifted in sequence {sequence}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. classification: the LocalZero rule (no cost == 0 heuristic)
// ---------------------------------------------------------------------------

const CLASSES: [ExecutionClass; 5] = [
    ExecutionClass::Local,
    ExecutionClass::HostedEconomy,
    ExecutionClass::HostedBalanced,
    ExecutionClass::Frontier,
    ExecutionClass::Unknown,
];

fn authorities() -> [Option<PriceAuthority>; 5] {
    [
        None,
        Some(PriceAuthority::Exact),
        Some(PriceAuthority::ConservativeCeiling),
        Some(PriceAuthority::LocalZero),
        Some(PriceAuthority::Unknown),
    ]
}

fn spend_row(
    class: ExecutionClass,
    authority: Option<PriceAuthority>,
    cost: u64,
) -> DurableSpendRow {
    DurableSpendRow {
        session_id: 1,
        task_id: 2,
        reservation_id: 3,
        attempt_id: "attempt-1".into(),
        provider: "provider".into(),
        model: "model".into(),
        input_tokens: 0,
        output_tokens: 0,
        prefix_cache_tokens: 0,
        provider_cost_micro: cost,
        provider_reported_micro: cost,
        price_authority: authority,
        execution_class: class,
        state: "settled".into(),
        occurred_at_ms: 1,
        source_operation: "test".into(),
    }
}

/// The spec's classification rule, independent of the production fallback:
/// a durable class wins; otherwise only a route-time LocalZero authority
/// establishes locality; everything else is `Unknown` — a numeric zero on a
/// hosted or unpriced row is NEVER local.
fn expected_class(class: ExecutionClass, authority: Option<PriceAuthority>) -> ExecutionClass {
    if class.is_known() {
        class
    } else {
        match authority {
            Some(PriceAuthority::LocalZero) => ExecutionClass::Local,
            _ => ExecutionClass::Unknown,
        }
    }
}

/// Exhaustive property over class x authority x cost: the durable
/// classification matches the spec exactly, and a zero-cost row without a
/// LocalZero authority can never come out Local (the old `cost == 0` heuristic
/// mutation).
#[test]
fn effective_execution_class_never_infers_locality_from_a_numeric_zero() {
    for class in CLASSES {
        for authority in authorities() {
            for cost in [0u64, 1, 999_999] {
                let row = spend_row(class, authority, cost);
                let effective = row.effective_execution_class();
                assert_eq!(
                    effective,
                    expected_class(class, authority),
                    "class {class:?} authority {authority:?} cost {cost}: classification drifted"
                );
                if effective == ExecutionClass::Unknown {
                    assert!(
                        !effective.is_local() && !effective.is_hosted(),
                        "Unknown must never count as local or hosted"
                    );
                }
                // The adversarial headline: zero cost is not locality.
                if cost == 0 && authority != Some(PriceAuthority::LocalZero) && !class.is_known() {
                    assert_ne!(
                        effective,
                        ExecutionClass::Local,
                        "a zero-cost row without LocalZero authority was classified LOCAL"
                    );
                }
            }
        }
    }
}

/// Deterministic witness pinning the two named adversarial rows: a
/// zero-cost unpriced row and a zero-cost Exact-priced (hosted free-tier)
/// row are both Unknown — never local.
#[test]
fn localzero_classification_is_pinned_against_the_cost_zero_heuristic() {
    let unpriced = spend_row(ExecutionClass::Unknown, None, 0);
    assert_eq!(
        unpriced.effective_execution_class(),
        ExecutionClass::Unknown
    );
    let hosted_zero = spend_row(ExecutionClass::Unknown, Some(PriceAuthority::Exact), 0);
    assert_eq!(
        hosted_zero.effective_execution_class(),
        ExecutionClass::Unknown
    );
    assert!(!hosted_zero.effective_execution_class().is_local());
    assert!(!hosted_zero.effective_execution_class().is_hosted());
    let local = spend_row(ExecutionClass::Unknown, Some(PriceAuthority::LocalZero), 0);
    assert_eq!(local.effective_execution_class(), ExecutionClass::Local);
    let known = spend_row(ExecutionClass::HostedBalanced, None, 0);
    assert_eq!(
        known.effective_execution_class(),
        ExecutionClass::HostedBalanced
    );
}

/// The route-time authority mapping itself: only LocalZero establishes
/// locality; every other authority leaves the class Unknown.
#[test]
fn route_time_authority_mapping_is_localzero_only() {
    for authority in [
        PriceAuthority::Exact,
        PriceAuthority::ConservativeCeiling,
        PriceAuthority::Unknown,
    ] {
        assert_eq!(
            ExecutionClass::from_price_authority(authority),
            ExecutionClass::Unknown,
            "{authority:?} must not establish locality"
        );
    }
    assert_eq!(
        ExecutionClass::from_price_authority(PriceAuthority::LocalZero),
        ExecutionClass::Local
    );
}
