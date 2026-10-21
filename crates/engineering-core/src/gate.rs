use engineering_units::{Dimension, Interval, Uncertainty};
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::CoreResult;
use crate::graph::NodeId;
use crate::receipt::{AnalysisReceipt, ExecutionEvidence};
use crate::value::{Provenance, ValueState};

/// Model responses are untrusted text: every established gate bounds the
/// strings and evidence lists it accepts so a hostile payload cannot pass a
/// gate (or bloat a durable transcript) merely by being long.
fn bounded(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max
}

fn bounded_evidence(values: &[String], max: usize) -> bool {
    !values.is_empty() && values.iter().all(|value| bounded(value, max))
}

fn unknown_solver_version(version: &str) -> bool {
    let version = version.trim().to_ascii_lowercase();
    version == "unknown" || version.ends_with("/unknown")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateId {
    Formulation,
    Authority,
    Theory,
    Validity,
    Dimensions,
    ExecutedAnalysis,
    CrossCheck,
    Uncertainty,
    CounterexampleSearch,
    Realization,
    Verification,
}

impl std::fmt::Display for GateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl GateId {
    pub const ALL: [Self; 11] = [
        Self::Formulation,
        Self::Authority,
        Self::Theory,
        Self::Validity,
        Self::Dimensions,
        Self::ExecutedAnalysis,
        Self::CrossCheck,
        Self::Uncertainty,
        Self::CounterexampleSearch,
        Self::Realization,
        Self::Verification,
    ];

    pub const fn as_str(self) -> &'static str {
        self.name()
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Formulation => "1. problem formulation",
            Self::Authority => "2. knowledge authority",
            Self::Theory => "3. theory selection",
            Self::Validity => "4. validity envelope",
            Self::Dimensions => "5. dimensional consistency",
            Self::ExecutedAnalysis => "6. executed analysis",
            Self::CrossCheck => "7. independent cross-check",
            Self::Uncertainty => "8. sensitivity/uncertainty",
            Self::CounterexampleSearch => "9. counterexample/failure search",
            Self::Realization => "10. realization consequences",
            Self::Verification => "11. falsifiable verification",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateGap {
    pub reason: String,
    pub missing: Vec<String>,
}

impl GateGap {
    pub fn new(reason: impl Into<String>, missing: Vec<String>) -> Self {
        Self {
            reason: reason.into(),
            missing,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.reason.trim().is_empty() && self.missing.iter().all(|item| item.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchRequest {
    pub gate: GateId,
    pub reason: String,
    pub sources_required: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateState<T> {
    Established(T),
    Unavailable(GateGap),
    /// The honest outcome when a governing theory or its validity envelope is
    /// not established: research, never improvisation.
    ResearchRequired(ResearchRequest),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gate<T> {
    pub state: GateState<T>,
}

impl<T> Gate<T> {
    pub fn established(value: T) -> Self {
        Self {
            state: GateState::Established(value),
        }
    }

    pub fn unavailable(reason: impl Into<String>, missing: Vec<String>) -> Self {
        Self {
            state: GateState::Unavailable(GateGap::new(reason, missing)),
        }
    }

    pub fn research_required(
        gate: GateId,
        reason: impl Into<String>,
        sources_required: Vec<String>,
    ) -> Self {
        Self {
            state: GateState::ResearchRequired(ResearchRequest {
                gate,
                reason: reason.into(),
                sources_required,
            }),
        }
    }

    pub fn established_value(&self) -> Option<&T> {
        match &self.state {
            GateState::Established(value) => Some(value),
            GateState::Unavailable(_) | GateState::ResearchRequired(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssumptionRecord {
    pub id: String,
    pub statement: String,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Formulation {
    pub objective: String,
    pub scope: String,
    pub requirements: Vec<NodeId>,
    pub constraints: Vec<NodeId>,
    pub assumptions: Vec<AssumptionRecord>,
    pub unknowns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorityRef {
    pub pack_id: String,
    pub version: String,
    pub digest: Digest,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeAuthority {
    pub packs: Vec<AuthorityRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSelection {
    pub model_id: String,
    pub version: String,
    pub equation_ids: Vec<String>,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TheorySelection {
    pub models: Vec<ModelSelection>,
    pub alternatives_considered: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidityEnvelope {
    pub model_id: String,
    pub temperature: Option<Interval>,
    pub conditions: Vec<String>,
    pub exclusions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidityEnvelopes {
    pub envelopes: Vec<ValidityEnvelope>,
}

/// One machine-checked dimensional equation. `passed` is always recomputed
/// from `lhs == rhs` during validation; a caller cannot assert it by prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DimensionCheck {
    expression: String,
    lhs: Dimension,
    rhs: Dimension,
    passed: bool,
}

impl DimensionCheck {
    pub fn new(expression: impl Into<String>, lhs: Dimension, rhs: Dimension) -> CoreResult<Self> {
        let expression = expression.into();
        if expression.trim().is_empty() {
            return Err(crate::error::CoreError::EmptyField {
                field: "dimension_check.expression",
            });
        }
        Ok(Self {
            passed: lhs == rhs,
            expression,
            lhs,
            rhs,
        })
    }

    pub fn expression(&self) -> &str {
        &self.expression
    }

    pub fn lhs(&self) -> Dimension {
        self.lhs
    }

    pub fn rhs(&self) -> Dimension {
        self.rhs
    }

    pub fn passed(&self) -> bool {
        self.passed && self.lhs == self.rhs
    }

    pub fn is_consistent(&self) -> bool {
        self.lhs == self.rhs
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DimensionChecks {
    pub checks: Vec<DimensionCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutedAnalysis {
    pub receipts: Vec<AnalysisReceipt>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agreement {
    pub metric: String,
    pub value: f64,
    pub tolerance: f64,
    pub agrees: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrossCheck {
    pub method: String,
    /// The primary method this check is independent of (solver id).
    pub independent_of: String,
    pub receipts: Vec<AnalysisReceipt>,
    pub agreement: Agreement,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UncertaintyContribution {
    pub source: String,
    pub uncertainty: Uncertainty,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UncertaintyStatement {
    pub method: String,
    pub contributions: Vec<UncertaintyContribution>,
    pub coverage: Option<f64>,
    pub summary: Option<Interval>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOutcome {
    NoFailureFound,
    FailureFound,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CounterexampleProbe {
    pub probe: String,
    pub regime: String,
    pub outcome: ProbeOutcome,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CounterexampleSearch {
    pub probes: Vec<CounterexampleProbe>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RealizationCategory {
    Manufacturing,
    Reliability,
    Sourcing,
    Cost,
    Test,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RealizationConsequence {
    pub category: RealizationCategory,
    pub statement: String,
    pub availability: ValueState<String>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RealizationConsequences {
    pub consequences: Vec<RealizationConsequence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FalsifiableTest {
    pub claim: String,
    pub procedure: String,
    pub pass_criterion: String,
    pub refutation: String,
    pub instrument: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FalsifiableVerification {
    pub tests: Vec<FalsifiableTest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineeringResponse {
    pub formulation: Gate<Formulation>,
    pub authority: Gate<KnowledgeAuthority>,
    pub theory: Gate<TheorySelection>,
    pub validity: Gate<ValidityEnvelopes>,
    pub dimensions: Gate<DimensionChecks>,
    pub executed_analysis: Gate<ExecutedAnalysis>,
    pub cross_check: Gate<CrossCheck>,
    pub uncertainty: Gate<UncertaintyStatement>,
    pub counterexample_search: Gate<CounterexampleSearch>,
    pub realization: Gate<RealizationConsequences>,
    pub verification: Gate<FalsifiableVerification>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateViolation {
    pub gate: GateId,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ResponseAssessment {
    pub established_gates: usize,
    pub violations: Vec<GateViolation>,
    pub research_required: Vec<ResearchRequest>,
}

impl ResponseAssessment {
    pub fn is_ready(&self) -> bool {
        self.established_gates == GateId::ALL.len()
            && self.violations.is_empty()
            && self.research_required.is_empty()
    }

    pub fn violation(&mut self, gate: GateId, message: impl Into<String>) {
        self.violations.push(GateViolation {
            gate,
            message: message.into(),
        });
    }

    pub fn research(&mut self, gate: GateId, gap: GateGap) {
        self.research_required.push(ResearchRequest {
            gate,
            reason: gap.reason,
            sources_required: gap.missing,
        });
    }
}

impl EngineeringResponse {
    /// Validate the 11-gate contract. Gates 5 and 6 are only established when
    /// they carry machine-checked dimensional equations and executed analysis
    /// receipts with consistent digests/evidence. An unavailable theory or
    /// validity envelope produces a typed research request, never a pass.
    pub fn assess(&self) -> ResponseAssessment {
        let mut out = ResponseAssessment::default();

        self.assess_formulation(&mut out);
        self.assess_authority(&mut out);
        self.assess_theory(&mut out);
        self.assess_validity(&mut out);
        self.assess_dimensions(&mut out);
        self.assess_execution(&mut out);
        self.assess_cross_check(&mut out);
        self.assess_uncertainty(&mut out);
        self.assess_counterexamples(&mut out);
        self.assess_realization(&mut out);
        self.assess_verification(&mut out);

        out
    }

    pub fn is_ready(&self) -> bool {
        self.assess().is_ready()
    }

    pub fn require_ready(&self) -> Result<(), ResponseAssessment> {
        let assessment = self.assess();
        if assessment.is_ready() {
            Ok(())
        } else {
            Err(assessment)
        }
    }

    /// Alias for [`Self::require_ready`], matching the conventional
    /// "validate this contract" call shape.
    pub fn validate(&self) -> Result<(), ResponseAssessment> {
        self.require_ready()
    }

    fn assess_formulation(&self, out: &mut ResponseAssessment) {
        match &self.formulation.state {
            GateState::Established(value) => {
                if !bounded(&value.objective, 8_192) || !bounded(&value.scope, 8_192) {
                    out.violation(GateId::Formulation, "objective and scope must be explicit");
                    return;
                }
                if value.requirements.is_empty() {
                    out.violation(
                        GateId::Formulation,
                        "at least one requirement must be bound to the graph",
                    );
                    return;
                }
                if value
                    .unknowns
                    .iter()
                    .any(|unknown| !bounded(unknown, 1_024))
                {
                    out.violation(GateId::Formulation, "unknowns must be explicit and bounded");
                    return;
                }
                for assumption in &value.assumptions {
                    if !bounded(&assumption.id, 256)
                        || !bounded(&assumption.statement, 8_192)
                        || assumption.provenance.validate().is_err()
                    {
                        out.violation(
                            GateId::Formulation,
                            "assumptions require id, statement and provenance",
                        );
                        return;
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::Formulation,
                format!("formulation unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => {
                out.research_required.push(request.clone());
            }
        }
    }

    fn assess_authority(&self, out: &mut ResponseAssessment) {
        match &self.authority.state {
            GateState::Established(value) => {
                if value.packs.is_empty() {
                    out.violation(GateId::Authority, "no knowledge pack authority is cited");
                    return;
                }
                if value.packs.iter().any(|pack| {
                    !bounded(&pack.pack_id, 256)
                        || !bounded(&pack.version, 128)
                        || !bounded(&pack.scope, 4_096)
                        || pack.digest == Digest::from_bytes([0; 32])
                }) {
                    out.violation(
                        GateId::Authority,
                        "each authority must name pack id, version and scope",
                    );
                    return;
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::Authority,
                format!("knowledge authority unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_theory(&self, out: &mut ResponseAssessment) {
        match &self.theory.state {
            GateState::Established(value) => {
                if value.models.is_empty() {
                    out.violation(GateId::Theory, "no governing model selected");
                    return;
                }
                if value.models.iter().any(|model| {
                    !bounded(&model.model_id, 256)
                        || !bounded(&model.version, 128)
                        || !bounded(&model.rationale, 8_192)
                        || !bounded_evidence(&model.equation_ids, 256)
                }) {
                    out.violation(
                        GateId::Theory,
                        "each model needs id, version, equation ids and a rationale",
                    );
                    return;
                }
                if !bounded_evidence(&value.alternatives_considered, 4_096) {
                    out.violation(
                        GateId::Theory,
                        "theory selection must record the alternatives considered",
                    );
                    return;
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.research(GateId::Theory, gap.clone()),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_validity(&self, out: &mut ResponseAssessment) {
        match &self.validity.state {
            GateState::Established(value) => {
                if value.envelopes.is_empty() {
                    out.violation(GateId::Validity, "no validity envelope recorded");
                    return;
                }
                for envelope in &value.envelopes {
                    if !bounded(&envelope.model_id, 256)
                        || (envelope.temperature.is_none()
                            && envelope.conditions.is_empty()
                            && envelope.exclusions.is_empty())
                        || envelope
                            .conditions
                            .iter()
                            .chain(envelope.exclusions.iter())
                            .any(|value| !bounded(value, 1_024))
                    {
                        out.violation(
                            GateId::Validity,
                            "each envelope must name a model and at least one bound, condition or exclusion",
                        );
                        return;
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.research(GateId::Validity, gap.clone()),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_dimensions(&self, out: &mut ResponseAssessment) {
        match &self.dimensions.state {
            GateState::Established(value) => {
                if value.checks.is_empty() {
                    out.violation(
                        GateId::Dimensions,
                        "no machine-checked dimensional equation is recorded",
                    );
                    return;
                }
                for check in &value.checks {
                    if !bounded(check.expression(), 4_096) {
                        out.violation(GateId::Dimensions, "dimension check has no expression");
                        return;
                    }
                    if !check.is_consistent() {
                        out.violation(
                            GateId::Dimensions,
                            format!(
                                "{} is dimensionally inconsistent: {} != {}",
                                check.expression(),
                                check.lhs(),
                                check.rhs()
                            ),
                        );
                        return;
                    }
                    if !check.passed() {
                        out.violation(
                            GateId::Dimensions,
                            format!("{} is marked failed", check.expression()),
                        );
                        return;
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::Dimensions,
                format!("dimensional checking unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_execution(&self, out: &mut ResponseAssessment) {
        match &self.executed_analysis.state {
            GateState::Established(value) => {
                if value.receipts.is_empty() {
                    out.violation(
                        GateId::ExecutedAnalysis,
                        "no executed analysis receipt is recorded",
                    );
                    return;
                }
                for receipt in &value.receipts {
                    if let Err(error) = receipt.validate() {
                        out.violation(
                            GateId::ExecutedAnalysis,
                            format!("invalid receipt: {error}"),
                        );
                        return;
                    }
                    if unknown_solver_version(&receipt.identity.solver_version) {
                        out.violation(
                            GateId::ExecutedAnalysis,
                            "receipt names an unknown solver version; version authority is required",
                        );
                        return;
                    }
                    if !receipt.convergence.converged {
                        out.violation(
                            GateId::ExecutedAnalysis,
                            "receipt records a non-converged analysis",
                        );
                        return;
                    }
                    if let ExecutionEvidence::SupervisedProcess {
                        exit_code,
                        timed_out,
                        output_truncated,
                        ..
                    } = &receipt.execution
                    {
                        if *timed_out {
                            out.violation(
                                GateId::ExecutedAnalysis,
                                "solver process exceeded its deadline",
                            );
                            return;
                        }
                        if *output_truncated {
                            out.violation(
                                GateId::ExecutedAnalysis,
                                "solver output was truncated; the verdict cannot be accepted",
                            );
                            return;
                        }
                        if *exit_code != 0 {
                            out.violation(
                                GateId::ExecutedAnalysis,
                                format!("solver process exited with {exit_code}"),
                            );
                            return;
                        }
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::ExecutedAnalysis,
                format!("executed analysis unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_cross_check(&self, out: &mut ResponseAssessment) {
        match &self.cross_check.state {
            GateState::Established(value) => {
                if !bounded(&value.method, 512) || !bounded(&value.independent_of, 256) {
                    out.violation(
                        GateId::CrossCheck,
                        "cross-check must name its method and the primary method it is independent of",
                    );
                    return;
                }
                if value.receipts.is_empty() {
                    out.violation(GateId::CrossCheck, "cross-check has no executed receipt");
                    return;
                }
                if bounded(&value.agreement.metric, 256) {
                    if !value.agreement.value.is_finite()
                        || !value.agreement.tolerance.is_finite()
                        || value.agreement.tolerance <= 0.0
                    {
                        out.violation(
                            GateId::CrossCheck,
                            "agreement value/tolerance must be finite and tolerance > 0",
                        );
                        return;
                    }
                    if !value.agreement.agrees
                        || value.agreement.value.abs() > value.agreement.tolerance
                    {
                        out.violation(
                            GateId::CrossCheck,
                            "independent methods disagree beyond the stated tolerance",
                        );
                        return;
                    }
                } else {
                    out.violation(GateId::CrossCheck, "agreement metric is missing");
                    return;
                }
                let primary_ids: std::collections::BTreeSet<&str> =
                    if let GateState::Established(executed) = &self.executed_analysis.state {
                        executed
                            .receipts
                            .iter()
                            .map(|receipt| receipt.identity.solver_id.as_str())
                            .collect()
                    } else {
                        std::collections::BTreeSet::new()
                    };
                for receipt in &value.receipts {
                    if let Err(error) = receipt.validate() {
                        out.violation(
                            GateId::CrossCheck,
                            format!("cross-check receipt invalid: {error}"),
                        );
                        return;
                    }
                    if unknown_solver_version(&receipt.identity.solver_version) {
                        out.violation(
                            GateId::CrossCheck,
                            "cross-check receipt names an unknown solver version",
                        );
                        return;
                    }
                    if !receipt.convergence.converged {
                        out.violation(
                            GateId::CrossCheck,
                            "cross-check receipt records a non-converged analysis",
                        );
                        return;
                    }
                    if let ExecutionEvidence::SupervisedProcess {
                        exit_code,
                        timed_out,
                        output_truncated,
                        ..
                    } = &receipt.execution
                    {
                        if *timed_out || *output_truncated || *exit_code != 0 {
                            out.violation(
                                GateId::CrossCheck,
                                "cross-check process did not complete cleanly",
                            );
                            return;
                        }
                    }
                    if primary_ids.contains(receipt.identity.solver_id.as_str())
                        || receipt.identity.solver_id == value.independent_of
                    {
                        out.violation(
                            GateId::CrossCheck,
                            "cross-check receipt is not independent of the primary method",
                        );
                        return;
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::CrossCheck,
                format!("cross-check unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_uncertainty(&self, out: &mut ResponseAssessment) {
        match &self.uncertainty.state {
            GateState::Established(value) => {
                if !bounded(&value.method, 256) {
                    out.violation(GateId::Uncertainty, "uncertainty method is missing");
                    return;
                }
                if value.contributions.is_empty() {
                    out.violation(
                        GateId::Uncertainty,
                        "no uncertainty contribution is recorded",
                    );
                    return;
                }
                if let Some(coverage) = value.coverage {
                    if !coverage.is_finite() || coverage <= 0.0 || coverage > 1.0 {
                        out.violation(GateId::Uncertainty, "coverage must lie in (0, 1]");
                        return;
                    }
                }
                for contribution in &value.contributions {
                    if !bounded(&contribution.source, 512) || !bounded(&contribution.note, 4_096) {
                        out.violation(
                            GateId::Uncertainty,
                            "each contribution needs a source and a note",
                        );
                        return;
                    }
                }
                if value.summary.is_none()
                    && value
                        .contributions
                        .iter()
                        .any(|contribution| !contribution.uncertainty.is_exact())
                {
                    out.violation(
                        GateId::Uncertainty,
                        "non-exact contributions require a numeric uncertainty summary",
                    );
                    return;
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::Uncertainty,
                format!("uncertainty unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_counterexamples(&self, out: &mut ResponseAssessment) {
        match &self.counterexample_search.state {
            GateState::Established(value) => {
                if value.probes.is_empty() {
                    out.violation(
                        GateId::CounterexampleSearch,
                        "no counterexample/failure probe was executed",
                    );
                    return;
                }
                for probe in &value.probes {
                    if !bounded(&probe.probe, 4_096) || !bounded(&probe.regime, 1_024) {
                        out.violation(
                            GateId::CounterexampleSearch,
                            "each probe needs a description and a regime",
                        );
                        return;
                    }
                    if probe.outcome != ProbeOutcome::NotApplicable
                        && !bounded_evidence(&probe.evidence, 1_024)
                    {
                        out.violation(
                            GateId::CounterexampleSearch,
                            "a probe verdict requires evidence",
                        );
                        return;
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::CounterexampleSearch,
                format!("counterexample search unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_realization(&self, out: &mut ResponseAssessment) {
        match &self.realization.state {
            GateState::Established(value) => {
                if value.consequences.is_empty() {
                    out.violation(
                        GateId::Realization,
                        "no realization consequence is recorded",
                    );
                    return;
                }
                for consequence in &value.consequences {
                    if !bounded(&consequence.statement, 8_192)
                        || !bounded_evidence(&consequence.evidence, 1_024)
                    {
                        out.violation(
                            GateId::Realization,
                            "each consequence needs a statement and evidence",
                        );
                        return;
                    }
                    if let ValueState::Known(text) = &consequence.availability {
                        if !bounded(text, 8_192) {
                            out.violation(
                                GateId::Realization,
                                "known availability must not be an empty string",
                            );
                            return;
                        }
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::Realization,
                format!("realization analysis unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }

    fn assess_verification(&self, out: &mut ResponseAssessment) {
        match &self.verification.state {
            GateState::Established(value) => {
                if value.tests.is_empty() {
                    out.violation(
                        GateId::Verification,
                        "no falsifiable verification is recorded",
                    );
                    return;
                }
                for test in &value.tests {
                    if !bounded(&test.claim, 8_192)
                        || !bounded(&test.procedure, 8_192)
                        || !bounded(&test.pass_criterion, 8_192)
                        || !bounded(&test.refutation, 8_192)
                        || !bounded(&test.instrument, 1_024)
                        || !bounded_evidence(&test.evidence, 1_024)
                    {
                        out.violation(
                            GateId::Verification,
                            "a falsifiable test needs claim, procedure, pass criterion, refutation, instrument and evidence",
                        );
                        return;
                    }
                }
                out.established_gates += 1;
            }
            GateState::Unavailable(gap) => out.violation(
                GateId::Verification,
                format!("verification unavailable: {}", gap.reason),
            ),
            GateState::ResearchRequired(request) => out.research_required.push(request.clone()),
        }
    }
}

/// Convenience: a response that stops at gate 3/4 because the authority is
/// missing. This is the typed alternative to guessing.
pub fn research_response(
    gate: GateId,
    reason: impl Into<String>,
    sources_required: Vec<String>,
) -> ResearchRequest {
    ResearchRequest {
        gate,
        reason: reason.into(),
        sources_required,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_check_recomputes_consistency() {
        let consistent =
            DimensionCheck::new("F = m a", Dimension::FORCE, Dimension::FORCE).unwrap();
        assert!(consistent.passed());
        let inconsistent = DimensionCheck::new("F = m", Dimension::FORCE, Dimension::MASS).unwrap();
        assert!(!inconsistent.passed());
    }

    #[test]
    fn research_request_names_the_gate_and_sources() {
        let request = research_response(
            GateId::Theory,
            "no governing model for creep at 900 K",
            vec!["validated creep model".to_string()],
        );
        assert_eq!(request.gate, GateId::Theory);
        assert!(!request.sources_required.is_empty());
    }

    #[test]
    fn an_empty_response_is_not_ready() {
        let response = EngineeringResponse {
            formulation: Gate::unavailable("not stated", vec![]),
            authority: Gate::unavailable("not stated", vec![]),
            theory: Gate::research_required(
                GateId::Theory,
                "no theory",
                vec!["textbook".to_string()],
            ),
            validity: Gate::research_required(
                GateId::Validity,
                "no envelope",
                vec!["datasheet".to_string()],
            ),
            dimensions: Gate::established(DimensionChecks { checks: vec![] }),
            executed_analysis: Gate::established(ExecutedAnalysis { receipts: vec![] }),
            cross_check: Gate::unavailable("none", vec![]),
            uncertainty: Gate::unavailable("none", vec![]),
            counterexample_search: Gate::unavailable("none", vec![]),
            realization: Gate::unavailable("none", vec![]),
            verification: Gate::unavailable("none", vec![]),
        };
        let assessment = response.assess();
        assert!(!assessment.is_ready());
        assert_eq!(assessment.research_required.len(), 2);
        assert!(!assessment.violations.is_empty());
    }
}
