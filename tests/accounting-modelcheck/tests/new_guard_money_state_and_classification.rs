//! NEW-GUARD adversarial verification (economic/measurement/classification
//! layers, owned by the mutation campaign): model-based property tests plus
//! deterministic planted-violation witnesses for two guards that the
//! standard suite does not pin:
//!
//! 1. the durable settlement lifecycle's transition legality
//!    (`crates/store/src/outcome_ledger.rs`): only
//!    `pending -> complete|unavailable`, `unavailable -> complete|pending`
//!    (a recovered read) and `complete -> complete` corrections are legal; a
//!    `complete -> complete` correction repairs measured amounts UPWARD only
//!    (every measured amount stays present and non-decreasing, and a measured
//!    avoided-hosted counterfactual stays measured), so a downward or
//!    re-unmeasuring correction is a typed Conflict with the monotone-repair
//!    message. Once the money is `complete` the projection is TERMINAL — no
//!    later `pending` or `unavailable` event may ever land, and an illegal
//!    append must leave the latest row byte-identical;
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

/// The spec's monotone-repair rule for a `complete -> complete` correction,
/// written independently of the production `settlement_correction_monotone`:
/// every measured amount present on the latest event stays present and may
/// only move UP, and a measured avoided-hosted counterfactual never becomes
/// unmeasured.
fn correction_monotone(latest: &EconomicSettlementRow, next: &EconomicSettlementRow) -> bool {
    let amount_upward = |previous: Option<i64>, candidate: Option<i64>| match (previous, candidate)
    {
        (Some(previous), Some(candidate)) => candidate >= previous,
        (Some(_), None) => false,
        (None, _) => true,
    };
    amount_upward(latest.model_spend_micro, next.model_spend_micro)
        && amount_upward(
            latest.verification_spend_micro,
            next.verification_spend_micro,
        )
        && amount_upward(latest.rework_spend_micro, next.rework_spend_micro)
        && amount_upward(latest.avoided_hosted_micro, next.avoided_hosted_micro)
        && (!latest.avoided_hosted_measured || next.avoided_hosted_measured)
}

/// How a `complete -> complete` submission relates to the monotone repair
/// law. The generators count these classes to prove that legal upward
/// corrections and both refusal shapes (downward amounts, re-unmeasured
/// counterfactual) were actually exercised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CorrectionClass {
    Admitted,
    Downward,
    Unmeasured,
}

fn correction_class(
    latest: &EconomicSettlementRow,
    next: &EconomicSettlementRow,
) -> CorrectionClass {
    if latest.avoided_hosted_measured && !next.avoided_hosted_measured {
        CorrectionClass::Unmeasured
    } else if correction_monotone(latest, next) {
        CorrectionClass::Admitted
    } else {
        CorrectionClass::Downward
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

/// The model's expectation for one submitted event: a verbatim replay of the
/// latest fact is a no-op; every lifecycle-legality refusal AND every
/// non-monotone `complete -> complete` correction is a typed Conflict; only
/// legal transitions with monotone repairs append.
fn expect_append(latest: Option<&ModelRow>, next: &EconomicSettlementRow) -> Expect {
    match latest {
        None => Expect::Appended(0),
        Some(latest) => {
            if semantically_equal(&latest.event, next) {
                Expect::Duplicate
            } else if !legal_transition(latest.state, &next.state) {
                Expect::Conflict
            } else if latest.state == "complete"
                && correction_class(&latest.event, next) != CorrectionClass::Admitted
            {
                // Only reachable for `complete -> complete` corrections: the
                // lifecycle check above admits exactly that state pair here.
                Expect::Conflict
            } else {
                Expect::Appended(latest.seq + 1)
            }
        }
    }
}

/// A VALID store row of `state` (the store's own `validate` rules hold by
/// construction; `salt` keeps same-state events semantically distinct).
fn settlement(run_id: &str, state: &str, salt: i64) -> EconomicSettlementRow {
    let complete = state == "complete";
    let model = 100 + salt;
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
        avoided_hosted_micro: complete.then_some(7 + salt),
        avoided_hosted_measured: complete,
        steps_json: if complete { steps } else { "[]".to_string() },
        settled_at_ms: 1_000 + salt,
    }
}

/// A VALID `complete` row whose avoided-hosted counterfactual is UNMEASURED
/// (the P1 measurement rule lets a measured counterfactual APPEAR later but
/// never disappear). Submitted after a measured complete row it is a refused
/// re-unmeasuring attempt; after an unmeasured/non-complete projection it is
/// a legal correction.
fn settlement_unmeasured(run_id: &str, salt: i64) -> EconomicSettlementRow {
    let model = 100 + salt;
    let steps = format!(r#"[{{"provider":"p","model":"m","local":false,"spend_micro":{model}}}]"#);
    EconomicSettlementRow {
        run_id: run_id.to_string(),
        seq: 0,
        schema_version: 1,
        state: "complete".to_string(),
        reason: format!("complete-unmeasured#{salt}"),
        model_spend_micro: Some(model),
        verification_spend_micro: Some(0),
        rework_spend_micro: Some(0),
        avoided_hosted_micro: None,
        avoided_hosted_measured: false,
        steps_json: steps,
        settled_at_ms: 1_000 + salt,
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
            // The two refusals are deliberately distinct: the lifecycle
            // message names the illegal state move, the monotone message
            // names the downward / re-unmeasured correction.
            let monotone_refusal = latest.as_ref().is_some_and(|latest| {
                latest.state == "complete"
                    && submitted.state == "complete"
                    && correction_class(&latest.event, submitted) != CorrectionClass::Admitted
            });
            if monotone_refusal {
                assert!(
                    message.contains("repair measured amounts upward only"),
                    "{context}: a non-monotone correction must be refused with the monotone \
                     message, got {message:?}"
                );
            } else {
                assert!(
                    message.contains("may not move from its current state into"),
                    "{context}: an illegal transition must be refused with the lifecycle \
                     message, got {message:?}"
                );
            }
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
/// accepted event may change the state away from `complete`, and a correction
/// may only repair measured amounts upward (never downward, never
/// re-unmeasuring the counterfactual).
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
    // complete -> complete UPWARD correction is still legal (the guard must
    // not over-block): repairing rework and the measured counterfactual
    // upward appends exactly one event.
    let mut corrected = complete.clone();
    corrected.reason = "correction".into();
    corrected.rework_spend_micro = Some(5);
    corrected.avoided_hosted_micro = Some(9);
    drive(
        &store,
        run,
        &mut latest,
        &corrected,
        "complete -> complete upward correction",
    );
    assert_eq!(latest.as_ref().unwrap().seq, 2);
    // complete -> complete DOWNWARD repairs and re-unmeasuring the
    // counterfactual are typed monotone conflicts that leave the latest row
    // byte-identical.
    let mut downward_rework = corrected.clone();
    downward_rework.reason = "downward-rework".into();
    downward_rework.rework_spend_micro = Some(4);
    let mut downward_model = corrected.clone();
    downward_model.reason = "downward-model".into();
    downward_model.model_spend_micro = Some(100);
    downward_model.steps_json =
        r#"[{"provider":"p","model":"m","local":false,"spend_micro":100}]"#.to_string();
    let mut downward_counterfactual = corrected.clone();
    downward_counterfactual.reason = "downward-counterfactual".into();
    downward_counterfactual.avoided_hosted_micro = Some(8);
    let mut re_unmeasured = corrected.clone();
    re_unmeasured.reason = "re-unmeasure".into();
    re_unmeasured.avoided_hosted_micro = None;
    re_unmeasured.avoided_hosted_measured = false;
    let before = store.economic_settlement_latest(run).unwrap().unwrap();
    for (label, attempt) in [
        ("downward rework", &downward_rework),
        ("downward model", &downward_model),
        ("downward counterfactual", &downward_counterfactual),
        ("re-unmeasured counterfactual", &re_unmeasured),
    ] {
        match store.economic_settlement_append(attempt) {
            Err(StoreError::Conflict(message)) => assert!(
                message.contains("repair measured amounts upward only"),
                "complete -> complete {label} must be refused with the monotone message, got \
                 {message:?}"
            ),
            other => panic!("complete -> complete {label} must be refused typed, got {other:?}"),
        }
        let after = store.economic_settlement_latest(run).unwrap().unwrap();
        assert_eq!(
            before, after,
            "a refused {label} correction must leave the latest row untouched"
        );
    }
    // complete -> pending and complete -> unavailable are typed lifecycle
    // conflicts and the terminal row is byte-identical afterwards.
    for state in ["pending", "unavailable"] {
        let regress = settlement(run, state, 9);
        match store.economic_settlement_append(&regress) {
            Err(StoreError::Conflict(message)) => assert!(
                message.contains("may not move from its current state into"),
                "complete -> {state} must be refused with the lifecycle message, got {message:?}"
            ),
            other => panic!("complete -> {state} must be refused typed, got {other:?}"),
        }
        let after = store.economic_settlement_latest(run).unwrap().unwrap();
        assert_eq!(
            before, after,
            "a refused complete -> {state} append must leave the latest row untouched"
        );
    }
}

/// The amount motion of one step of the exhaustive corpus: the salt walk
/// (and with it every measured amount, which follows the salt) goes UP,
/// DOWN, or the event arrives with an UNMEASURED counterfactual. On
/// non-complete steps there are no amounts, so the motion only walks the
/// salt and keeps the fresh rows semantically distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    Up,
    Down,
    Unmeasured,
}

const MOTIONS: [Motion; 3] = [Motion::Up, Motion::Down, Motion::Unmeasured];

impl Motion {
    /// The next salt of the walk.
    fn walk(self, salt: i64) -> i64 {
        match self {
            Motion::Up | Motion::Unmeasured => salt + 1,
            Motion::Down => salt - 1,
        }
    }

    /// The fresh event of this motion for `state`.
    fn event(self, run: &str, state: &str, salt: i64) -> EconomicSettlementRow {
        if state == "complete" && self == Motion::Unmeasured {
            settlement_unmeasured(run, salt)
        } else {
            settlement(run, state, salt)
        }
    }
}

/// Advance a mixed-radix counter over `radix`; `false` reports the wrap.
fn advance(counter: &mut [usize], radix: usize) -> bool {
    for slot in counter.iter_mut().rev() {
        *slot += 1;
        if *slot < radix {
            return true;
        }
        *slot = 0;
    }
    false
}

/// The exhaustive bounded corpus (every state sequence of length 1..=3
/// crossed with every amount-motion pattern, 9 + 81 + 729 = 819 sequences)
/// plus seeded longer interleavings (fresh up/down/unmeasuring motions,
/// verbatim replays of earlier submitted rows): every append agrees with the
/// SPEC model, only legal transitions are accepted, both legal upward
/// corrections and illegal downward / re-unmeasuring attempts are exercised,
/// and terminal immutability holds at every step.
#[test]
fn settlement_lifecycle_only_legal_transitions_over_exhaustive_and_seeded_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();

    // Exhaustive: the motion dimension guarantees the corpus drives legal
    // upward corrections (accepted) and downward / re-unmeasuring attempts
    // (refused); the class counters below prove both were actually seen.
    let mut exhausted_upward = 0u32;
    let mut exhausted_downward = 0u32;
    let mut exhausted_unmeasured = 0u32;
    let mut ordinal = 0usize;
    for length in 1..=3usize {
        let mut states = vec![0usize; length];
        loop {
            let mut motions = vec![0usize; length];
            loop {
                let run = format!("run-exh-{ordinal}");
                let mut latest: Option<ModelRow> = None;
                let mut salt = 100i64;
                for (position, index) in states.iter().enumerate() {
                    let motion = MOTIONS[motions[position]];
                    salt = motion.walk(salt);
                    let state = STATES[*index];
                    let event = motion.event(&run, state, salt);
                    if let Some(previous) = &latest {
                        if previous.state == "complete"
                            && state == "complete"
                            && !semantically_equal(&previous.event, &event)
                        {
                            match correction_class(&previous.event, &event) {
                                CorrectionClass::Admitted => exhausted_upward += 1,
                                CorrectionClass::Downward => exhausted_downward += 1,
                                CorrectionClass::Unmeasured => exhausted_unmeasured += 1,
                            }
                        }
                    }
                    drive(
                        &store,
                        &run,
                        &mut latest,
                        &event,
                        &format!(
                            "exhaustive states {states:?} motions {motions:?} step {position}"
                        ),
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
                if !advance(&mut motions, MOTIONS.len()) {
                    break;
                }
            }
            if !advance(&mut states, STATES.len()) {
                break;
            }
        }
    }
    assert_eq!(
        ordinal, 819,
        "the exhaustive corpus must cover every state sequence x motion pattern"
    );
    assert!(
        exhausted_upward > 0,
        "the exhaustive corpus must exercise accepted upward corrections"
    );
    assert!(
        exhausted_downward > 0,
        "the exhaustive corpus must exercise refused downward corrections"
    );
    assert!(
        exhausted_unmeasured > 0,
        "the exhaustive corpus must exercise refused re-unmeasuring corrections"
    );

    // Seeded longer interleavings: fresh events (with a deliberately chosen
    // up/down/unmeasuring motion, so upward corrections and downward attempts
    // are both common) plus verbatim replays of earlier submitted rows (the
    // store compares against the LATEST row only, so a replay of an older
    // fact can itself be a downward correction attempt).
    let mut seeded_upward = 0u32;
    let mut seeded_downward = 0u32;
    let mut seeded_unmeasured = 0u32;
    for sequence in 0..120u64 {
        let mut lcg = Lcg::new(0x5E77_1E00_0000_0001 ^ sequence.wrapping_mul(0x9E37_79B9));
        let run = format!("run-seeded-{sequence}");
        let mut latest: Option<ModelRow> = None;
        let mut submitted: Vec<EconomicSettlementRow> = Vec::new();
        let steps = 2 + lcg.below(9);
        let mut salt = 10_000i64 + 100 * sequence as i64;
        for position in 0..steps {
            let event = if position > 0 && lcg.chance(30) {
                submitted[lcg.below(submitted.len() as u64) as usize].clone()
            } else {
                let state = STATES[lcg.below(STATES.len() as u64) as usize];
                let motion = if lcg.chance(50) {
                    if state == "complete" && lcg.chance(25) {
                        Motion::Unmeasured
                    } else {
                        Motion::Up
                    }
                } else {
                    Motion::Down
                };
                salt = motion.walk(salt);
                motion.event(&run, state, salt)
            };
            if let Some(previous) = &latest {
                if previous.state == "complete"
                    && event.state == "complete"
                    && !semantically_equal(&previous.event, &event)
                {
                    match correction_class(&previous.event, &event) {
                        CorrectionClass::Admitted => seeded_upward += 1,
                        CorrectionClass::Downward => seeded_downward += 1,
                        CorrectionClass::Unmeasured => seeded_unmeasured += 1,
                    }
                }
            }
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
    assert!(
        seeded_upward > 0,
        "the seeded corpus must exercise accepted upward corrections"
    );
    assert!(
        seeded_downward > 0,
        "the seeded corpus must exercise refused downward corrections"
    );
    assert!(
        seeded_unmeasured > 0,
        "the seeded corpus must exercise refused re-unmeasuring corrections"
    );
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
