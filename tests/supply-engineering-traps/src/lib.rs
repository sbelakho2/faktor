//! Vicious trap benchmark for Faktor Supply + Engineering.
//!
//! Every case is deterministic and bounded. A case either executes a real
//! knowledge-pack-backed model/solver/unit kernel, or it returns the typed
//! research-required/refusal/unavailable/escalation the missing authority
//! demands. No missing physical data is replaced by a plausible number, and
//! no benchmark fixture is presented as authoritative external evidence.
//!
//! The machine-readable report records the eight review dimensions required
//! by `docs/specs/supply-engineering.md`: correct physics, correct authority,
//! missing-information detection, boundary conditions, uncertainty, trap
//! detection, valid falsifiable experiment construction, and willingness to
//! refuse.

use engineering_core::{FalsifiableTest, GateId};
use faktor_supply_core::Money;
use serde::Serialize;
use thiserror::Error;

pub mod cases;
pub mod fixtures;

pub const SUITE_SCHEMA: &str = "faktor.supply-engineering-traps/v1";
pub const SUITE_VERSION: &str = "1.0.0";
pub const MAX_CASES: usize = 32;
pub const MAX_EVIDENCE: usize = 64;
pub const MAX_MISSING_INFO: usize = 64;
pub const MAX_BOUNDARIES: usize = 64;
pub const MAX_OBSERVATIONS: usize = 128;
pub const MAX_TEXT: usize = 4_096;

/// A refused benchmark operation.
#[derive(Debug, Error)]
pub enum BenchError {
    /// The case id is empty.
    #[error("case id must not be empty")]
    EmptyCaseId,
    /// The case id is not part of this suite.
    #[error("unknown trap case {0:?}")]
    UnknownCase(String),
    /// A caller asked for more cases than the fixed bound.
    #[error("case request exceeds the bound of {MAX_CASES}")]
    CaseBound,
    /// A case could not execute its real model/fixture.
    #[error("case {id} failed: {detail}")]
    Case {
        /// Case id.
        id: String,
        /// Deterministic detail.
        detail: String,
    },
    /// A report field exceeded its bound.
    #[error("report field {0} exceeds its bound")]
    ReportBound(&'static str),
    /// JSON serialization failed.
    #[error("report serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// The typed result a trap case is expected to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedKind {
    /// The model executed and produced a usable result.
    Solved,
    /// The model executed and rejected the design/claim through a required
    /// check.
    Rejected,
    /// The model returned a typed refusal.
    Refused,
    /// The authority/theory/validity needed for a conclusion is missing.
    ResearchRequired,
    /// A value is unknown and remains unavailable.
    Unavailable,
    /// Conflicting authoritative inputs require escalation.
    Escalated,
}

impl ExpectedKind {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Solved => "solved",
            Self::Rejected => "rejected",
            Self::Refused => "refused",
            Self::ResearchRequired => "research_required",
            Self::Unavailable => "unavailable",
            Self::Escalated => "escalated",
        }
    }
}

/// One observed value, kept typed so money never becomes a float.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Observation {
    /// A physical quantity in its explicit unit.
    Quantity {
        /// Name.
        name: String,
        /// Numeric value.
        value: f64,
        /// Unit symbol.
        unit: String,
    },
    /// Exact integer money.
    Money {
        /// Name.
        name: String,
        /// Micro-units.
        micros: i64,
        /// Currency code.
        currency: String,
    },
    /// A bounded text observation.
    Text {
        /// Name.
        name: String,
        /// Value.
        value: String,
    },
}

impl Observation {
    /// A physical quantity observation.
    pub fn quantity(name: impl Into<String>, value: f64, unit: impl Into<String>) -> Self {
        Self::Quantity {
            name: name.into(),
            value,
            unit: unit.into(),
        }
    }

    /// An exact money observation.
    pub fn money(name: impl Into<String>, amount: Money) -> Self {
        Self::Money {
            name: name.into(),
            micros: amount.micros(),
            currency: amount.currency.as_str().to_string(),
        }
    }

    /// A text observation.
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Text {
            name: name.into(),
            value: value.into(),
        }
    }

    /// True for exact money observations.
    pub const fn is_money(&self) -> bool {
        matches!(self, Self::Money { .. })
    }
}

/// The typed outcome of executing one case.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaseOutcome {
    /// The model executed and produced observations.
    Solved {
        /// Observations.
        observations: Vec<Observation>,
        /// Explanatory notes.
        notes: Vec<String>,
    },
    /// The model executed and the design failed a required check.
    Rejected {
        /// Stable rejection code.
        code: String,
        /// Why the design was rejected.
        reasons: Vec<String>,
    },
    /// The model returned a typed refusal.
    Refused {
        /// Stable refusal code.
        code: String,
        /// Why it refused.
        reason: String,
        /// Missing or invalid inputs named by the refusal.
        required: Vec<String>,
        /// True when the refusal is bound to an executable receipt.
        receipt_bound: bool,
    },
    /// A theory, authority or validity envelope is missing.
    ResearchRequired {
        /// The gate that cannot be established.
        gate: GateId,
        /// Why research is required.
        reason: String,
        /// Sources needed to proceed.
        sources_required: Vec<String>,
    },
    /// A value is unknown and stays unavailable.
    Unavailable {
        /// Why the value is unavailable.
        reason: String,
        /// Inputs needed to establish it.
        required: Vec<String>,
    },
    /// Conflicting authoritative inputs require escalation.
    Escalated {
        /// Stable escalation kind.
        escalation: String,
        /// Reason.
        reason: String,
        /// Authority to consult.
        authority: String,
    },
}

impl CaseOutcome {
    /// The outcome class.
    pub const fn kind(&self) -> ExpectedKind {
        match self {
            Self::Solved { .. } => ExpectedKind::Solved,
            Self::Rejected { .. } => ExpectedKind::Rejected,
            Self::Refused { .. } => ExpectedKind::Refused,
            Self::ResearchRequired { .. } => ExpectedKind::ResearchRequired,
            Self::Unavailable { .. } => ExpectedKind::Unavailable,
            Self::Escalated { .. } => ExpectedKind::Escalated,
        }
    }
}

/// The eight review dimensions the suite measures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Metrics {
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
}

impl Metrics {
    /// All eight dimensions pass.
    pub const fn all(&self) -> bool {
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
    pub const fn count(&self) -> usize {
        self.correct_physics as usize
            + self.authority as usize
            + self.missing_info_detected as usize
            + self.boundary_conditions as usize
            + self.uncertainty as usize
            + self.trap_detected as usize
            + self.falsifiable_experiment as usize
            + self.refused_where_appropriate as usize
    }

    /// Builder: correct physics.
    pub const fn with_physics(mut self, value: bool) -> Self {
        self.correct_physics = value;
        self
    }

    /// Builder: authority.
    pub const fn with_authority(mut self, value: bool) -> Self {
        self.authority = value;
        self
    }

    /// Builder: missing information.
    pub const fn with_missing_info(mut self, value: bool) -> Self {
        self.missing_info_detected = value;
        self
    }

    /// Builder: boundary conditions.
    pub const fn with_boundaries(mut self, value: bool) -> Self {
        self.boundary_conditions = value;
        self
    }

    /// Builder: uncertainty.
    pub const fn with_uncertainty(mut self, value: bool) -> Self {
        self.uncertainty = value;
        self
    }

    /// Builder: trap detection.
    pub const fn with_trap(mut self, value: bool) -> Self {
        self.trap_detected = value;
        self
    }

    /// Builder: falsifiable experiment.
    pub const fn with_experiment(mut self, value: bool) -> Self {
        self.falsifiable_experiment = value;
        self
    }

    /// Builder: refusal/rejection/escalation.
    pub const fn with_refusal(mut self, value: bool) -> Self {
        self.refused_where_appropriate = value;
        self
    }
}

/// One trap case result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseReport {
    /// Stable case id.
    pub id: String,
    /// Domain pack / discipline.
    pub domain: String,
    /// Short title.
    pub title: String,
    /// What a shallow answer would claim.
    pub shallow_claim: String,
    /// The expected outcome class.
    pub expected: ExpectedKind,
    /// The actual typed outcome.
    pub outcome: CaseOutcome,
    /// The measured review dimensions.
    pub metrics: Metrics,
    /// Evidence/digest/citation strings.
    pub evidence: Vec<String>,
    /// Missing information named by the case.
    pub missing_info: Vec<String>,
    /// Boundary/validity conditions named by the case.
    pub boundary_conditions: Vec<String>,
    /// Explicit uncertainty statement.
    pub uncertainty_statement: String,
    /// The falsifiable experiment that could disprove the result.
    pub falsifiable_test: FalsifiableTest,
}

impl CaseReport {
    /// True when all eight dimensions pass and the expected outcome matches.
    pub fn passed(&self) -> bool {
        self.metrics.all() && self.outcome.kind() == self.expected
    }
}

/// Builder for a bounded [`CaseReport`].
#[derive(Debug)]
pub struct CaseBuilder {
    id: String,
    domain: String,
    title: String,
    shallow_claim: String,
    expected: ExpectedKind,
    outcome: Option<CaseOutcome>,
    metrics: Metrics,
    evidence: Vec<String>,
    missing_info: Vec<String>,
    boundary_conditions: Vec<String>,
    uncertainty_statement: String,
    falsifiable_test: Option<FalsifiableTest>,
}

impl CaseBuilder {
    /// Start a case.
    pub fn new(
        id: impl Into<String>,
        domain: impl Into<String>,
        title: impl Into<String>,
        expected: ExpectedKind,
    ) -> Self {
        Self {
            id: id.into(),
            domain: domain.into(),
            title: title.into(),
            shallow_claim: String::new(),
            expected,
            outcome: None,
            metrics: Metrics::default(),
            evidence: Vec::new(),
            missing_info: Vec::new(),
            boundary_conditions: Vec::new(),
            uncertainty_statement: String::new(),
            falsifiable_test: None,
        }
    }

    /// Set the shallow claim.
    pub fn shallow(mut self, claim: impl Into<String>) -> Self {
        self.shallow_claim = claim.into();
        self
    }

    /// Set the typed outcome.
    pub fn outcome(mut self, outcome: CaseOutcome) -> Self {
        self.outcome = Some(outcome);
        self
    }

    /// Set the metrics.
    pub const fn metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// Add evidence lines.
    pub fn evidence(mut self, items: impl IntoIterator<Item = String>) -> Self {
        self.evidence.extend(items);
        self
    }

    /// Add missing-information lines.
    pub fn missing(mut self, items: impl IntoIterator<Item = String>) -> Self {
        self.missing_info.extend(items);
        self
    }

    /// Add boundary-condition lines.
    pub fn boundaries(mut self, items: impl IntoIterator<Item = String>) -> Self {
        self.boundary_conditions.extend(items);
        self
    }

    /// Set the uncertainty statement.
    pub fn uncertainty(mut self, statement: impl Into<String>) -> Self {
        self.uncertainty_statement = statement.into();
        self
    }

    /// Set the falsifiable test.
    pub fn test(mut self, test: FalsifiableTest) -> Self {
        self.falsifiable_test = Some(test);
        self
    }

    /// Build and enforce the report bounds.
    pub fn build(self) -> Result<CaseReport, BenchError> {
        let outcome = self.outcome.ok_or_else(|| BenchError::Case {
            id: self.id.clone(),
            detail: "case produced no outcome".to_string(),
        })?;
        let falsifiable_test = self.falsifiable_test.ok_or_else(|| BenchError::Case {
            id: self.id.clone(),
            detail: "case produced no falsifiable experiment".to_string(),
        })?;
        let report = CaseReport {
            id: self.id,
            domain: self.domain,
            title: self.title,
            shallow_claim: self.shallow_claim,
            expected: self.expected,
            outcome,
            metrics: self.metrics,
            evidence: self.evidence,
            missing_info: self.missing_info,
            boundary_conditions: self.boundary_conditions,
            uncertainty_statement: self.uncertainty_statement,
            falsifiable_test,
        };
        validate_report(&report)?;
        Ok(report)
    }
}

fn validate_report(report: &CaseReport) -> Result<(), BenchError> {
    if report.id.trim().is_empty() || report.id.len() > MAX_TEXT {
        return Err(BenchError::EmptyCaseId);
    }
    if report.evidence.len() > MAX_EVIDENCE {
        return Err(BenchError::ReportBound("evidence"));
    }
    if report.missing_info.len() > MAX_MISSING_INFO {
        return Err(BenchError::ReportBound("missing_info"));
    }
    if report.boundary_conditions.len() > MAX_BOUNDARIES {
        return Err(BenchError::ReportBound("boundary_conditions"));
    }
    if let CaseOutcome::Solved { observations, .. } = &report.outcome {
        if observations.len() > MAX_OBSERVATIONS {
            return Err(BenchError::ReportBound("observations"));
        }
    }
    if report.uncertainty_statement.trim().is_empty() {
        return Err(BenchError::ReportBound("uncertainty_statement"));
    }
    if report.shallow_claim.trim().is_empty() {
        return Err(BenchError::ReportBound("shallow_claim"));
    }
    if report.falsifiable_test.procedure.trim().is_empty()
        || report.falsifiable_test.pass_criterion.trim().is_empty()
        || report.falsifiable_test.refutation.trim().is_empty()
    {
        return Err(BenchError::ReportBound("falsifiable_test"));
    }
    Ok(())
}

/// Aggregate pass/fail state for the eight metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MetricPass {
    /// Cases passing correct physics.
    pub correct_physics: usize,
    /// Cases passing authority.
    pub authority: usize,
    /// Cases passing missing-information detection.
    pub missing_info_detected: usize,
    /// Cases passing boundary conditions.
    pub boundary_conditions: usize,
    /// Cases passing uncertainty.
    pub uncertainty: usize,
    /// Cases passing trap detection.
    pub trap_detected: usize,
    /// Cases passing falsifiable experiment construction.
    pub falsifiable_experiment: usize,
    /// Cases passing refusal/rejection where appropriate.
    pub refused_where_appropriate: usize,
}

/// Suite summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Total cases.
    pub total: usize,
    /// Cases whose metrics all pass and whose outcome matches.
    pub passed: usize,
    /// Cases that failed.
    pub failed: usize,
    /// Outcomes by class.
    pub solved: usize,
    /// Rejected designs.
    pub rejected: usize,
    /// Typed refusals.
    pub refused: usize,
    /// Research-required outcomes.
    pub research_required: usize,
    /// Unavailable outcomes.
    pub unavailable: usize,
    /// Escalations.
    pub escalated: usize,
    /// Per-metric pass counts.
    pub metric_pass: MetricPass,
    /// Failed case ids.
    pub failed_cases: Vec<String>,
}

impl Summary {
    /// True only when every case passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.passed == self.total && self.total > 0
    }
}

/// The full machine-readable suite report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuiteReport {
    /// Report schema.
    pub schema: String,
    /// Suite version.
    pub version: String,
    /// Case results in deterministic dispatch order.
    pub cases: Vec<CaseReport>,
    /// Aggregate summary.
    pub summary: Summary,
}

impl SuiteReport {
    /// Build a report from cases, enforcing the suite bound.
    pub fn from_cases(cases: Vec<CaseReport>) -> Result<Self, BenchError> {
        if cases.is_empty() {
            return Err(BenchError::CaseBound);
        }
        if cases.len() > MAX_CASES {
            return Err(BenchError::CaseBound);
        }
        let summary = summarize(&cases);
        Ok(Self {
            schema: SUITE_SCHEMA.to_string(),
            version: SUITE_VERSION.to_string(),
            cases,
            summary,
        })
    }

    /// JSON bytes (pretty or compact) with deterministic field order.
    pub fn to_json(&self, pretty: bool) -> Result<String, BenchError> {
        if pretty {
            Ok(serde_json::to_string_pretty(self)?)
        } else {
            Ok(serde_json::to_string(self)?)
        }
    }
}

fn summarize(cases: &[CaseReport]) -> Summary {
    let mut summary = Summary {
        total: cases.len(),
        passed: 0,
        failed: 0,
        solved: 0,
        rejected: 0,
        refused: 0,
        research_required: 0,
        unavailable: 0,
        escalated: 0,
        metric_pass: MetricPass::default(),
        failed_cases: Vec::new(),
    };
    for case in cases {
        match case.outcome.kind() {
            ExpectedKind::Solved => summary.solved += 1,
            ExpectedKind::Rejected => summary.rejected += 1,
            ExpectedKind::Refused => summary.refused += 1,
            ExpectedKind::ResearchRequired => summary.research_required += 1,
            ExpectedKind::Unavailable => summary.unavailable += 1,
            ExpectedKind::Escalated => summary.escalated += 1,
        }
        if case.metrics.correct_physics {
            summary.metric_pass.correct_physics += 1;
        }
        if case.metrics.authority {
            summary.metric_pass.authority += 1;
        }
        if case.metrics.missing_info_detected {
            summary.metric_pass.missing_info_detected += 1;
        }
        if case.metrics.boundary_conditions {
            summary.metric_pass.boundary_conditions += 1;
        }
        if case.metrics.uncertainty {
            summary.metric_pass.uncertainty += 1;
        }
        if case.metrics.trap_detected {
            summary.metric_pass.trap_detected += 1;
        }
        if case.metrics.falsifiable_experiment {
            summary.metric_pass.falsifiable_experiment += 1;
        }
        if case.metrics.refused_where_appropriate {
            summary.metric_pass.refused_where_appropriate += 1;
        }
        if case.passed() {
            summary.passed += 1;
        } else {
            summary.failed += 1;
            summary.failed_cases.push(case.id.clone());
        }
    }
    summary
}

/// Run every case in the fixed suite order.
pub fn run_suite() -> Result<SuiteReport, BenchError> {
    let cases = case_ids()
        .iter()
        .map(|id| run_case(id))
        .collect::<Result<Vec<_>, _>>()?;
    SuiteReport::from_cases(cases)
}

/// Run one case by id.
pub fn run_case(id: &str) -> Result<CaseReport, BenchError> {
    if id.trim().is_empty() {
        return Err(BenchError::EmptyCaseId);
    }
    cases::dispatch(id)
}

/// The stable case ids in dispatch order.
pub fn case_ids() -> Vec<&'static str> {
    cases::CASE_IDS.to_vec()
}
