//! Independent evaluator for the trap benchmark.
//!
//! The oracle never imports the SUT module, the engineering model/solver
//! kernels or the supply cost/logistics kernels. It owns its expected
//! numerical windows, refusal codes, gate states and assumption coverage and
//! checks the opaque [`SutAnswer`](crate::contract::SutAnswer) against them.
//! Knowledge-pack authority is re-read from the bundled registry, so a SUT
//! claim that a pack covers (or does not cover) a regime is verified
//! independently.

use engineering_core::{FalsifiableTest, GateId};
use engineering_knowledge::{KnowledgePack, KnowledgeRegistry};
use serde::Serialize;

use crate::contract::{ExpectedKind, GateStateKind, SutAnswer, SutOutcome, SutUncertainty};

mod expectations;

pub use expectations::{expectation, CASE_COUNT, CASE_IDS};

/// One numeric observation window.
#[derive(Debug, Clone, Copy)]
pub struct QuantityWindow {
    /// Observation name.
    pub name: &'static str,
    /// Inclusive lower bound.
    pub lower: f64,
    /// Inclusive upper bound.
    pub upper: f64,
}

/// One exact money window in micro-units.
#[derive(Debug, Clone, Copy)]
pub struct MoneyWindow {
    /// Observation name.
    pub name: &'static str,
    /// Inclusive lower bound.
    pub lower: i64,
    /// Inclusive upper bound.
    pub upper: i64,
}

/// How authority is independently verified.
#[derive(Debug, Clone, Copy)]
pub enum AuthorityExpectation {
    /// These exact packs/items must exist in the bundled registry.
    PackItems(&'static [(&'static str, &'static str)]),
    /// No bundled pack may contain any of these token groups. Each inner
    /// slice is a set of alternatives; at least one alternative must be
    /// absent from every pack.
    NoPackCoverage(&'static [&'static [&'static str]]),
    /// The pack item text must contain every listed token.
    PackText {
        /// Pack id.
        pack_id: &'static str,
        /// Item id.
        item_id: &'static str,
        /// Tokens that must all appear.
        tokens: &'static [&'static str],
    },
    /// Only the answer's recorded assumption/evidence coverage is checked.
    AnswerCoverage,
}

/// Expected uncertainty shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UncertaintyExpectation {
    /// An exact result.
    Exact,
    /// An explicit interval.
    Interval,
    /// A typed absence of an estimate.
    NotEstimated,
}

/// The shallow claim's contradiction rule.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapExpectation {
    /// The answer must not be `Solved`.
    pub requires_non_solved: bool,
    /// A quantity observation must lie outside the shallow window.
    pub quantity_outside: Option<QuantityWindow>,
    /// A money observation must exceed this micro-unit threshold.
    pub money_above: Option<(&'static str, i64)>,
    /// A named control must refuse with this code.
    pub control_refused: Option<(&'static str, &'static str)>,
}

impl TrapExpectation {
    /// No trap rule.
    pub const DEFAULT: Self = Self {
        requires_non_solved: false,
        quantity_outside: None,
        money_above: None,
        control_refused: None,
    };
}

/// One expected control outcome.
#[derive(Debug, Clone, Copy)]
pub struct ControlExpectation {
    /// Control name.
    pub name: &'static str,
    /// Expected kind.
    pub kind: ExpectedKind,
    /// Expected refusal code, when any.
    pub refusal_code: Option<&'static str>,
    /// Expected uncertainty shape, when the control solved.
    pub uncertainty: Option<UncertaintyExpectation>,
}

/// The oracle's complete expectation for one case.
#[derive(Debug, Clone, Copy)]
pub struct OracleExpectation {
    /// Expected outcome class.
    pub kind: ExpectedKind,
    /// Acceptable refusal/rejection codes (empty for non-refusal kinds).
    pub refusal_codes: &'static [&'static str],
    /// Required numeric observation windows.
    pub windows: &'static [QuantityWindow],
    /// Required exact money windows.
    pub money: &'static [MoneyWindow],
    /// Observation names that must be absent (no fabricated value).
    pub absent: &'static [&'static str],
    /// Independent authority check.
    pub authority: AuthorityExpectation,
    /// Missing-information token groups. Each group needs one hit.
    pub missing: &'static [&'static [&'static str]],
    /// Boundary-condition token groups. Each group needs one hit.
    pub boundaries: &'static [&'static [&'static str]],
    /// Expected uncertainty shape.
    pub uncertainty: UncertaintyExpectation,
    /// Trap contradiction rule.
    pub trap: TrapExpectation,
    /// Experiment token groups. Each group needs one hit across the tests.
    pub experiments: &'static [&'static [&'static str]],
    /// Required gate states from the 11-gate response.
    pub gates: &'static [(GateId, GateStateKind)],
    /// Expected control probes.
    pub controls: &'static [ControlExpectation],
    /// Coverage token groups that must appear in the answer's coverage lines.
    pub coverage: &'static [&'static [&'static str]],
}

/// The oracle's eight-dimensional verdict. Only [`evaluate`] constructs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OracleVerdict {
    /// The result matches the physically/legally correct behaviour.
    pub correct_physics: bool,
    /// The result is bound to a real pack/solver/unit/evidence authority.
    pub authority: bool,
    /// Missing information is detected and named rather than filled in.
    pub missing_info_detected: bool,
    /// The governing boundary/validity condition is named and checked.
    pub boundary_conditions: bool,
    /// Uncertainty, or its typed absence, is explicit.
    pub uncertainty: bool,
    /// The shallow claim is contradicted by the executed case.
    pub trap_detected: bool,
    /// A concrete falsifiable experiment is constructed.
    pub falsifiable_experiment: bool,
    /// The case refuses/rejects/escalates where the correct behaviour does.
    pub refused_where_appropriate: bool,
    /// Names of the failed dimensions.
    pub failed_dimensions: Vec<String>,
    /// Deterministic explanation of each failure.
    pub failures: Vec<String>,
}

impl OracleVerdict {
    /// True only when all eight dimensions pass.
    pub fn all(&self) -> bool {
        self.correct_physics
            && self.authority
            && self.missing_info_detected
            && self.boundary_conditions
            && self.uncertainty
            && self.trap_detected
            && self.falsifiable_experiment
            && self.refused_where_appropriate
    }

    /// Count of passing dimensions.
    pub fn count(&self) -> usize {
        self.correct_physics as usize
            + self.authority as usize
            + self.missing_info_detected as usize
            + self.boundary_conditions as usize
            + self.uncertainty as usize
            + self.trap_detected as usize
            + self.falsifiable_experiment as usize
            + self.refused_where_appropriate as usize
    }
}

/// Evaluate an opaque SUT answer against the oracle's expectations.
pub fn evaluate(expectation: &OracleExpectation, answer: &SutAnswer) -> OracleVerdict {
    let registry = KnowledgeRegistry::bundled().ok();
    let mut verdict = OracleVerdict {
        correct_physics: false,
        authority: false,
        missing_info_detected: false,
        boundary_conditions: false,
        uncertainty: false,
        trap_detected: false,
        falsifiable_experiment: false,
        refused_where_appropriate: false,
        failed_dimensions: Vec::new(),
        failures: Vec::new(),
    };

    verdict.correct_physics = physics(expectation, answer, &mut verdict.failures);
    verdict.authority = authority(
        expectation,
        answer,
        registry.as_ref(),
        &mut verdict.failures,
    );
    verdict.missing_info_detected = token_groups(
        expectation.missing,
        |token| answer.mentions_missing(token),
        "missing",
        &mut verdict.failures,
    );
    verdict.boundary_conditions = token_groups(
        expectation.boundaries,
        |token| answer.mentions_boundary(token),
        "boundary",
        &mut verdict.failures,
    );
    verdict.uncertainty = uncertainty(expectation.uncertainty, &answer.uncertainty);
    if !verdict.uncertainty {
        verdict.failures.push(
            "uncertainty shape does not match the expected typed absence/interval".to_string(),
        );
    }
    verdict.trap_detected = trap(expectation.trap, answer);
    if !verdict.trap_detected {
        verdict
            .failures
            .push("the shallow claim was not contradicted by the SUT result".to_string());
    }
    verdict.falsifiable_experiment = experiments(expectation.experiments, &answer.experiments);
    if !verdict.falsifiable_experiment {
        verdict
            .failures
            .push("no complete falsifiable experiment covering the claim".to_string());
    }
    verdict.refused_where_appropriate = refusal(expectation, answer);
    if !verdict.refused_where_appropriate {
        verdict.failures.push(
            "the SUT did not refuse/reject/escalate/return unavailable where required".to_string(),
        );
    }
    for (name, passed) in [
        ("correct_physics", verdict.correct_physics),
        ("authority", verdict.authority),
        ("missing_info_detected", verdict.missing_info_detected),
        ("boundary_conditions", verdict.boundary_conditions),
        ("uncertainty", verdict.uncertainty),
        ("trap_detected", verdict.trap_detected),
        ("falsifiable_experiment", verdict.falsifiable_experiment),
        (
            "refused_where_appropriate",
            verdict.refused_where_appropriate,
        ),
    ] {
        if !passed {
            verdict.failed_dimensions.push(name.to_string());
        }
    }
    verdict
}

fn physics(
    expectation: &OracleExpectation,
    answer: &SutAnswer,
    failures: &mut Vec<String>,
) -> bool {
    let mut passed = true;
    if answer.outcome.kind() != expectation.kind {
        failures.push(format!(
            "outcome {:?} != expected {:?}",
            answer.outcome.kind(),
            expectation.kind
        ));
        passed = false;
    }
    if !expectation.refusal_codes.is_empty() {
        match answer.outcome.refusal_code() {
            Some(code) if expectation.refusal_codes.contains(&code) => {}
            other => {
                failures.push(format!(
                    "refusal code {other:?} not in {:?}",
                    expectation.refusal_codes
                ));
                passed = false;
            }
        }
    }
    for window in expectation.windows {
        match answer.quantity(window.name) {
            Some(value) if value.is_finite() && value >= window.lower && value <= window.upper => {}
            Some(value) => {
                failures.push(format!(
                    "{} = {value} outside [{}, {}]",
                    window.name, window.lower, window.upper
                ));
                passed = false;
            }
            None => {
                failures.push(format!("missing numeric observation {}", window.name));
                passed = false;
            }
        }
    }
    for window in expectation.money {
        match answer.money_micros(window.name) {
            Some(value) if value >= window.lower && value <= window.upper => {}
            Some(value) => {
                failures.push(format!(
                    "{} = {value} micros outside [{}, {}]",
                    window.name, window.lower, window.upper
                ));
                passed = false;
            }
            None => {
                failures.push(format!("missing money observation {}", window.name));
                passed = false;
            }
        }
    }
    for name in expectation.absent {
        if answer.observation(name).is_some() {
            failures.push(format!("fabricated observation {name}"));
            passed = false;
        }
    }
    passed
}

fn authority(
    expectation: &OracleExpectation,
    answer: &SutAnswer,
    registry: Option<&KnowledgeRegistry>,
    failures: &mut Vec<String>,
) -> bool {
    let mut passed = true;
    let Some(registry) = registry else {
        failures.push("bundled knowledge registry failed to load".to_string());
        return false;
    };
    match expectation.authority {
        AuthorityExpectation::PackItems(items) => {
            for (pack_id, item_id) in items {
                if !pack_has_item(registry, pack_id, item_id) {
                    failures.push(format!("authority item {pack_id}/{item_id} is missing"));
                    passed = false;
                }
            }
            for (pack_id, item_id) in items {
                if !answer.mentions_coverage(pack_id) || !answer.mentions_coverage(item_id) {
                    failures.push(format!(
                        "the answer does not record authority {pack_id}/{item_id}"
                    ));
                    passed = false;
                }
            }
        }
        AuthorityExpectation::NoPackCoverage(groups) => {
            if let Some(pack) = registry.packs().find(|pack| {
                groups.iter().any(|tokens| {
                    let text = pack_text(pack);
                    tokens.iter().all(|token| text.contains(token))
                })
            }) {
                failures.push(format!(
                    "pack {} unexpectedly covers the claim; research-required is wrong",
                    pack.id
                ));
                passed = false;
            }
        }
        AuthorityExpectation::PackText {
            pack_id,
            item_id,
            tokens,
        } => {
            let Some(pack) = registry.latest(pack_id) else {
                failures.push(format!("pack {pack_id} is missing"));
                return false;
            };
            let Some(text) = item_text(pack, item_id) else {
                failures.push(format!("item {pack_id}/{item_id} is missing"));
                return false;
            };
            for token in tokens {
                if !text.contains(token) {
                    failures.push(format!(
                        "pack item {pack_id}/{item_id} does not state {token:?}"
                    ));
                    passed = false;
                }
            }
        }
        AuthorityExpectation::AnswerCoverage => {}
    }
    if !token_groups(
        expectation.coverage,
        |token| answer.mentions_coverage(token),
        "coverage",
        failures,
    ) {
        passed = false;
    }
    passed
}

fn token_groups(
    groups: &[&[&str]],
    matches: impl Fn(&str) -> bool,
    dimension: &str,
    failures: &mut Vec<String>,
) -> bool {
    let mut passed = true;
    for group in groups {
        if !group.iter().any(|token| matches(token)) {
            failures.push(format!("{dimension} does not name any of {group:?}"));
            passed = false;
        }
    }
    passed
}

fn uncertainty(expected: UncertaintyExpectation, actual: &SutUncertainty) -> bool {
    match expected {
        UncertaintyExpectation::Exact => matches!(actual, SutUncertainty::Exact),
        UncertaintyExpectation::Interval => actual.is_interval(),
        UncertaintyExpectation::NotEstimated => actual.is_not_estimated(),
    }
}

fn trap(expected: TrapExpectation, answer: &SutAnswer) -> bool {
    let mut passed = true;
    if expected.requires_non_solved && answer.outcome.kind() == ExpectedKind::Solved {
        passed = false;
    }
    if let Some(window) = expected.quantity_outside {
        passed &= answer
            .quantity(window.name)
            .is_some_and(|value| value < window.lower || value > window.upper);
    }
    if let Some((name, threshold)) = expected.money_above {
        passed &= answer
            .money_micros(name)
            .is_some_and(|value| value > threshold);
    }
    if let Some((control, code)) = expected.control_refused {
        passed &= answer.control(control).is_some_and(|control| {
            control.outcome.kind() == ExpectedKind::Refused
                && control.outcome.refusal_code() == Some(code)
        });
    }
    passed
}

fn experiments(groups: &[&[&str]], tests: &[FalsifiableTest]) -> bool {
    if tests.is_empty() {
        return false;
    }
    if tests.iter().any(|test| {
        test.claim.trim().is_empty()
            || test.procedure.trim().is_empty()
            || test.pass_criterion.trim().is_empty()
            || test.refutation.trim().is_empty()
            || test.instrument.trim().is_empty()
            || test.evidence.is_empty()
    }) {
        return false;
    }
    let text = tests.iter().fold(String::new(), |mut text, test| {
        text.push_str(&test.claim);
        text.push(' ');
        text.push_str(&test.procedure);
        text.push(' ');
        text.push_str(&test.pass_criterion);
        text.push(' ');
        text.push_str(&test.refutation);
        text.push(' ');
        text.push_str(&test.instrument);
        for evidence in &test.evidence {
            text.push(' ');
            text.push_str(evidence);
        }
        text.push(' ');
        text
    });
    let text = text.to_ascii_lowercase();
    groups.iter().all(|group| {
        group
            .iter()
            .any(|token| text.contains(&token.to_ascii_lowercase()))
    })
}

fn refusal(expectation: &OracleExpectation, answer: &SutAnswer) -> bool {
    if answer.outcome.kind() != expectation.kind {
        return false;
    }
    match &answer.outcome {
        SutOutcome::Solved { .. } => expectation.controls.iter().all(|control| {
            answer.control(control.name).is_some_and(|answer| {
                answer.outcome.kind() == control.kind
                    && control
                        .refusal_code
                        .is_none_or(|code| answer.outcome.refusal_code() == Some(code))
                    && control
                        .uncertainty
                        .is_none_or(|expected| uncertainty(expected, &answer.uncertainty))
            })
        }),
        SutOutcome::Rejected { code, reasons, .. } => {
            !reasons.is_empty()
                && (expectation.refusal_codes.is_empty()
                    || expectation.refusal_codes.contains(&code.as_str()))
        }
        SutOutcome::Refused {
            code,
            reason,
            required,
        } => {
            !reason.trim().is_empty()
                && !required.is_empty()
                && (expectation.refusal_codes.is_empty()
                    || expectation.refusal_codes.contains(&code.as_str()))
        }
        SutOutcome::ResearchRequired {
            reason,
            sources_required,
            ..
        } => !reason.trim().is_empty() && !sources_required.is_empty(),
        SutOutcome::Unavailable { reason, required } => {
            !reason.trim().is_empty() && !required.is_empty()
        }
        SutOutcome::Ambiguous {
            candidates,
            reason,
            authority,
        } => candidates.len() >= 2 && !reason.trim().is_empty() && !authority.trim().is_empty(),
    }
}

fn pack_text(pack: &KnowledgePack) -> String {
    serde_json::to_string(pack)
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn pack_has_item(registry: &KnowledgeRegistry, pack_id: &str, item_id: &str) -> bool {
    registry
        .latest(pack_id)
        .is_some_and(|pack| item_text(pack, item_id).is_some())
}

fn item_text(pack: &KnowledgePack, item_id: &str) -> Option<String> {
    let value = serde_json::to_value(pack).ok()?;
    find_item(&value, item_id).map(|item| {
        let mut text = String::new();
        collect_strings(item, &mut text);
        text.to_ascii_lowercase()
    })
}

fn find_item<'a>(value: &'a serde_json::Value, id: &str) -> Option<&'a serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => items.iter().find_map(|item| find_item(item, id)),
        serde_json::Value::Object(map) => {
            if map.get("id").and_then(serde_json::Value::as_str) == Some(id) {
                return Some(value);
            }
            map.values().find_map(|item| find_item(item, id))
        }
        _ => None,
    }
}

fn collect_strings(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(text) => {
            out.push_str(text);
            out.push(' ');
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}
