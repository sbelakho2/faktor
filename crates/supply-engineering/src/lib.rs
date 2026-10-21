//! The closed Faktor Engineering + Faktor Supply loop.
//!
//! Engineering publishes a revision-bound requirement/constraint set. Supply
//! proposes typed candidates (part, supplier role/site, processes, quote,
//! should-cost and risk-adjusted TCO). Engineering evaluates every candidate
//! against the frozen set, derives falsifiable tests for unresolved claims,
//! and only then does Supply qualify, negotiate and plan. The result is one
//! [`SupplyDecision`] with explicit assumptions, uncertainty, evidence,
//! counterfactuals and a typed recommendation.
//!
//! The loop is pure: no model calls, no network, no persistence and no order
//! or message side effects. Orders and messages remain authorization-gated by
//! `faktor-supply-execution`; this crate never executes them.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use engineering_core::{
    evaluate_requirements, si_crosscheck_verdict, Agreement, AnalysisIdentity, AnalysisKind,
    AnalysisReceipt, ApplicabilitySelection, AssumptionRecord, AuthorityRef,
    CandidateEvaluation as EngineeringCandidateEvaluation, CandidateFacts, CatalogModel,
    CounterexampleProbe, CounterexampleSearch, CrossCheck, DerivedTest, Digest, DimensionChecks,
    Discipline, EngineeringRequirementSet, EngineeringResponse, EngineeringValue,
    EngineeringVerdict, ExecutedAnalysis, FalsifiableTest, FalsifiableVerification, Formulation,
    Gate, GateId, KnowledgeAuthority, ModelCatalog, ModelSelection, ModelSelector, NodeId,
    OutcomeState, ProbeOutcome, PropertyRevision, Provenance, ProvenanceKind, RealizationCategory,
    RealizationConsequence, RealizationConsequences, ReceiptContent, ReceiptResult, ReceiptSigner,
    ReceiptValue, ReceiptVerifier, RequirementEvidence, RequirementSelection, RevisionId,
    SelectionProvenance, SelectionRequest, TheorySelection, UncertaintyContribution,
    UncertaintyStatement, ValidityDomain, ValidityEnvelopes, ValueState,
};
use engineering_knowledge::{KnowledgeError, KnowledgePack, KnowledgeRegistry};
use engineering_units::{Quantity, Uncertainty};
use faktor_supply_core::{
    validate_text, Assumption, Availability, Basis, Candidate, CandidateId, CapabilitySummary,
    CommercialTermsSummary, Counterfactual, Day, EvidenceRef, ExactError, FitAssessment, Incoterm,
    InventorySummary, LandedCostSummary, LogisticsSummary, Money, PartId, ProcessKind, Rate,
    RecommendedAction, Revision, RiskLevel, RiskSummary, SiteId, SupplierId, SupplierRole,
    SupplyDecision, SupplyRequirement, TransportMode, UnavailableReason, UncertaintyClass,
    UncertaintyNote, Units, MAX_CANDIDATES, MAX_CONFLICTS, MAX_COUNTERFACTUALS,
    MAX_DECISION_ASSUMPTIONS, MAX_DECISION_EVIDENCE, MAX_ID_BYTES, MAX_MISSING_ITEMS,
    MAX_TEXT_BYTES,
};
use faktor_supply_cost::{
    should_cost, tco, CostError, ShouldCost, ShouldCostInput, Tco, TcoError, TcoInput,
};
use faktor_supply_execution::{
    batna, compare_bids, negotiation_plan, normalize_quote, Batna, BidComparison, ExecutionError,
    NegotiationPlan, NormalizationContext, NormalizedBid, Quote, RfqPackage,
};
use faktor_supply_supplier::{
    assess as assess_supplier, AuthorizationState, FitReport, FitVerdict, SourcingRequirement,
    SupplierProfile,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum candidates one loop may evaluate.
pub const MAX_LOOP_CANDIDATES: usize = 1_024;
/// Maximum candidate evaluations returned in one result.
pub const MAX_LOOP_RANKED: usize = 64;
/// Maximum knowledge domains requested.
pub const MAX_LOOP_KNOWLEDGE_DOMAINS: usize = 16;
/// Maximum applied relaxations.
pub const MAX_LOOP_RELAXATIONS: usize = 16;

/// Closed-loop error. Every variant is a refusal, never a fabricated result.
#[derive(Debug, Error)]
pub enum LoopError {
    #[error("engineering: {0}")]
    Engineering(#[from] engineering_core::CoreError),
    #[error("supply: {0}")]
    Supply(#[from] ExactError),
    #[error("should-cost: {0}")]
    Cost(#[from] CostError),
    #[error("TCO: {0}")]
    Tco(#[from] TcoError),
    #[error("supply execution: {0}")]
    Execution(#[from] ExecutionError),
    #[error("knowledge: {0}")]
    Knowledge(#[from] KnowledgeError),
    #[error("units: {0}")]
    Units(#[from] engineering_units::UnitError),
    #[error("invalid loop request: {0}")]
    Invalid(String),
}

/// Result alias.
pub type LoopResult<T> = Result<T, LoopError>;

/// Inventory position for the requirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryContext {
    /// On hand at the decision revision.
    pub on_hand: Units,
    /// Confirmed supply inside the horizon.
    pub confirmed: Units,
    /// Safety-stock target.
    pub safety_stock: Units,
}

/// Negotiation context. The loop normalizes quotes; it never sends one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NegotiationContext {
    /// Annual cost of capital.
    pub cost_of_capital: Rate,
    /// Payment days every bid is normalized to.
    pub baseline_payment_days: u32,
    /// Destination country for the RFQ package.
    pub destination_country: String,
    /// Requested Incoterm.
    pub requested_incoterm: Incoterm,
    /// Transport mode assumed for the RFQ package.
    pub transport_mode: TransportMode,
}

/// A typed engineering relaxation. The original requirement revision is
/// preserved; a successful relaxation publishes a new revision and its
/// evidence. Supply may attach revised should-cost, quote and TCO inputs;
/// they are used only when a relaxation actually applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relaxation {
    /// Requirement to relax.
    pub requirement_id: String,
    /// New numeric limit.
    pub new_limit: engineering_core::NumericLimit,
    /// Why the relaxation is justified.
    pub rationale: String,
    /// Source of the justification.
    pub provenance: Provenance,
    /// What would invalidate the relaxation.
    pub falsifier: String,
    /// Evidence binding the justification.
    pub evidence: RequirementEvidence,
}

/// One supply candidate with its engineering facts and commercial terms.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopCandidate {
    /// Candidate id.
    pub id: CandidateId,
    /// Supplier node.
    pub supplier: SupplierId,
    /// Supplier role at decision time.
    pub role: SupplierRole,
    /// Optional site.
    pub site: Option<SiteId>,
    /// Part node.
    pub part: Option<PartId>,
    /// Manufacturing processes claimed for the candidate.
    #[serde(default)]
    pub processes: Vec<String>,
    /// Engineering facts for the candidate/alternate.
    pub facts: CandidateFacts,
    /// Engineering-grade supplier profile, when assessed.
    pub supplier_profile: Option<SupplierProfile>,
    /// Quote at the requested quantity.
    pub quote: Option<Quote>,
    /// Revised quote used only when a relaxation applied.
    pub relaxed_quote: Option<Quote>,
    /// Should-cost input for the manufactured item.
    pub should_cost: Option<ShouldCostInput>,
    /// Revised should-cost used only when a relaxation applied.
    pub relaxed_should_cost: Option<ShouldCostInput>,
    /// Risk-adjusted TCO terms.
    pub tco: Option<TcoInput>,
    /// Revised TCO used only when a relaxation applied.
    pub relaxed_tco: Option<TcoInput>,
    /// Quoted/estimated lead time.
    pub lead_time_days: faktor_supply_core::Term<u32>,
    /// Transit days, when known.
    pub transit_days: Availability<u32>,
    /// Chargeable weight in grams, when known.
    pub chargeable_weight_g: Availability<u64>,
    /// Explicit quality risk, overriding the supplier-metric derivation.
    pub quality_risk: Option<RiskSummary>,
    /// Explicit continuity risk, overriding the supplier-profile derivation.
    pub continuity_risk: Option<RiskSummary>,
    /// Lifecycle risk when an observation exists.
    pub lifecycle_risk: Option<RiskSummary>,
    /// Candidate evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// One bounded loop request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopRequest {
    /// Decision id.
    pub decision_id: String,
    /// Supply decision revision.
    pub revision: Revision,
    /// Engineering requirement-set revision the request was built against.
    pub engineering_revision: RevisionId,
    /// Supply requirement.
    pub requirement: SupplyRequirement,
    /// Engineering requirement/constraint set.
    pub engineering: EngineeringRequirementSet,
    /// Supplier-capability requirement, when capability must be assessed.
    pub sourcing: Option<SourcingRequirement>,
    /// Require a supplier fit report before a candidate can be promoted.
    #[serde(default)]
    pub require_supplier_fit: bool,
    /// Candidates.
    pub candidates: Vec<LoopCandidate>,
    /// Knowledge domains whose packs are the cited authority.
    pub knowledge_domains: Vec<String>,
    /// Explicit assumptions.
    #[serde(default)]
    pub assumptions: Vec<Assumption>,
    /// Evidence bound to the request.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Inventory position.
    pub inventory: InventoryContext,
    /// Negotiation/normalization context.
    pub negotiation: NegotiationContext,
    /// Optional requirement relaxations, applied in order.
    #[serde(default)]
    pub relaxations: Vec<Relaxation>,
}

impl LoopRequest {
    /// Validate every bound and cross-reference.
    pub fn validate(&self) -> LoopResult<()> {
        validate_text(&self.decision_id, MAX_ID_BYTES, "decision id")?;
        if self.revision.get() == 0 {
            return Err(LoopError::Invalid(
                "supply decision revision must be non-zero".to_string(),
            ));
        }
        if self.engineering_revision.get() == 0 {
            return Err(LoopError::Invalid(
                "engineering revision must be non-zero".to_string(),
            ));
        }
        self.requirement.validate()?;
        self.engineering.validate()?;
        if self.engineering.revision != self.engineering_revision {
            return Err(LoopError::Invalid(format!(
                "request binds engineering revision {} but the set is revision {}",
                self.engineering_revision, self.engineering.revision
            )));
        }
        if self.engineering.models.is_empty() {
            return Err(LoopError::Invalid(
                "a closed-loop requirement set needs at least one governing model".to_string(),
            ));
        }
        if !self
            .engineering
            .requirements
            .iter()
            .any(|requirement| requirement.target.model().is_some())
        {
            return Err(LoopError::Invalid(
                "no requirement binds a governing model; the applicability selector has nothing to govern"
                    .to_string(),
            ));
        }
        if self.candidates.is_empty() || self.candidates.len() > MAX_LOOP_CANDIDATES {
            return Err(LoopError::Invalid(format!(
                "candidates must contain 1..={MAX_LOOP_CANDIDATES} entries, got {}",
                self.candidates.len()
            )));
        }
        if self.knowledge_domains.is_empty()
            || self.knowledge_domains.len() > MAX_LOOP_KNOWLEDGE_DOMAINS
        {
            return Err(LoopError::Invalid(format!(
                "knowledge domains must contain 1..={MAX_LOOP_KNOWLEDGE_DOMAINS} entries"
            )));
        }
        for domain in &self.knowledge_domains {
            validate_text(domain, MAX_ID_BYTES, "knowledge domain")?;
        }
        if self.relaxations.len() > MAX_LOOP_RELAXATIONS {
            return Err(LoopError::Invalid(format!(
                "relaxations exceed {MAX_LOOP_RELAXATIONS}"
            )));
        }
        if self.assumptions.len() > MAX_DECISION_ASSUMPTIONS {
            return Err(LoopError::Invalid(format!(
                "assumptions exceed {MAX_DECISION_ASSUMPTIONS}"
            )));
        }
        for assumption in &self.assumptions {
            assumption.validate()?;
        }
        for reference in &self.evidence {
            reference.validate()?;
        }
        let mut ids = BTreeSet::new();
        for candidate in &self.candidates {
            candidate.id.validate()?;
            if !ids.insert(candidate.id.0.clone()) {
                return Err(LoopError::Invalid(format!(
                    "duplicate candidate id {}",
                    candidate.id.0
                )));
            }
            candidate.facts.validate()?;
            if let Some(profile) = &candidate.supplier_profile {
                profile.validate()?;
            }
            if let Some(quote) = &candidate.quote {
                quote.validate()?;
            }
            if let Some(quote) = &candidate.relaxed_quote {
                quote.validate()?;
            }
            if let Some(input) = &candidate.should_cost {
                input.validate()?;
            }
            if let Some(input) = &candidate.relaxed_should_cost {
                input.validate()?;
            }
            if let Some(input) = &candidate.tco {
                input.validate()?;
            }
            if let Some(input) = &candidate.relaxed_tco {
                input.validate()?;
            }
            candidate.lead_time_days.validate()?;
            for reference in &candidate.evidence {
                reference.validate()?;
            }
        }
        for relaxation in &self.relaxations {
            validate_text(
                &relaxation.requirement_id,
                MAX_ID_BYTES,
                "relaxation requirement id",
            )?;
            relaxation.new_limit.validate()?;
            validate_text(
                &relaxation.rationale,
                MAX_TEXT_BYTES,
                "relaxation rationale",
            )?;
            validate_text(
                &relaxation.falsifier,
                MAX_TEXT_BYTES,
                "relaxation falsifier",
            )?;
            relaxation.provenance.validate()?;
            relaxation.evidence.validate()?;
        }
        if self.negotiation.baseline_payment_days > 3_650
            || self.negotiation.destination_country.trim().is_empty()
        {
            return Err(LoopError::Invalid(
                "negotiation context is invalid".to_string(),
            ));
        }
        Ok(())
    }
}

/// One requirement relaxed during this run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedRelaxation {
    /// Requirement id.
    pub requirement_id: String,
    /// Original revision.
    pub from_revision: RevisionId,
    /// New revision.
    pub to_revision: RevisionId,
    /// Original limit.
    pub from_limit: engineering_core::NumericLimit,
    /// New limit.
    pub to_limit: engineering_core::NumericLimit,
    /// Rationale.
    pub rationale: String,
    /// Evidence id.
    pub evidence: String,
}

/// Compact supplier-fit projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplierFitSummary {
    /// Verdict.
    pub verdict: FitVerdict,
    /// Authorization state.
    pub authorization: AuthorizationState,
    /// Satisfied checks.
    pub satisfied: usize,
    /// Missing checks.
    pub missing: usize,
    /// Violated checks.
    pub violated: usize,
    /// Surfaced conflicts.
    pub conflicts: usize,
    /// Evidence.
    pub evidence: Vec<EvidenceRef>,
}

impl SupplierFitSummary {
    fn from_report(report: &FitReport) -> Self {
        Self {
            verdict: report.verdict,
            authorization: report.authorization.state,
            satisfied: report.count("satisfied"),
            missing: report.count("missing"),
            violated: report.count("violated"),
            conflicts: report.conflicts.len(),
            evidence: report.evidence.clone(),
        }
    }

    fn promotable(&self) -> bool {
        matches!(
            self.verdict,
            FitVerdict::Fit | FitVerdict::FitWithConditions
        )
    }
}

/// One candidate's complete evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEvaluation {
    /// Candidate id.
    pub candidate: CandidateId,
    /// Supplier node.
    pub supplier: SupplierId,
    /// Supplier role.
    pub role: SupplierRole,
    /// Engineering evaluation over the frozen requirement set.
    pub engineering: EngineeringCandidateEvaluation,
    /// Supplier fit, when a profile and sourcing requirement were supplied.
    pub supplier_fit: Option<SupplierFitSummary>,
    /// Exact should-cost, when supplied.
    pub should_cost: Option<ShouldCost>,
    /// Landed TCO, when supplied.
    pub tco: Option<Tco>,
    /// Normalized quote, when a quote was supplied.
    pub normalized_bid: Option<NormalizedBid>,
    /// Landed per-unit cost.
    pub landed_per_unit: Availability<Money>,
    /// Landed total cost.
    pub landed_total: Availability<Money>,
    /// Lead time in days.
    pub lead_time_days: Availability<u32>,
    /// Quality risk.
    pub quality_risk: Availability<RiskSummary>,
    /// Continuity risk.
    pub continuity_risk: Availability<RiskSummary>,
    /// Lifecycle risk.
    pub lifecycle_risk: Availability<RiskSummary>,
    /// True only when every blocker is cleared.
    pub promotable: bool,
    /// Typed blockers.
    pub blockers: Vec<String>,
    /// Engineering tests plus knowledge-pack test methods.
    pub derived_tests: Vec<DerivedTest>,
    /// Knowledge test-method references used.
    pub knowledge_refs: Vec<String>,
}

/// Negotiation outcome for the decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NegotiationOutcome {
    /// RFQ package used for normalization.
    pub package: RfqPackage,
    /// Bid comparison.
    pub comparison: BidComparison,
    /// BATNA scaffold.
    pub batna: Batna,
    /// Negotiation plan.
    pub negotiation: NegotiationPlan,
}

/// The loop's typed digest binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopBinding {
    /// Digest of the request.
    pub input_digest: Digest,
    /// Digest of the result payload.
    pub result_digest: Digest,
    /// Engineering revision actually evaluated.
    pub engineering_revision: RevisionId,
    /// Supply decision revision.
    pub supply_revision: Revision,
}

/// The typed authorization notice. The loop never sends or orders anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationNotice {
    /// Orders and messages always require an explicit grant.
    pub orders_and_messages_require_grant: bool,
    /// Actions executed by this loop (always empty).
    pub executed_actions: Vec<String>,
}

/// Measured loop work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopPerformance {
    /// Candidates evaluated.
    pub evaluated_candidates: usize,
    /// Candidates promotable after all gates.
    pub promotable_candidates: usize,
    /// Candidates returned in this result.
    pub returned_candidates: usize,
    /// Wall time of the deterministic kernel.
    pub elapsed_micros: u64,
}

/// Verification of a stored loop result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopVerification {
    /// The decision validates.
    pub decision_valid: bool,
    /// The requirement set validates.
    pub requirement_set_valid: bool,
    /// The analysis receipt validates and binds the request.
    pub receipt_valid: bool,
    /// The result digest recomputes.
    pub result_digest_matches: bool,
    /// The 11-gate response is ready, when one was produced.
    pub response_ready: bool,
    /// No order or message action was executed.
    pub no_ungated_actions: bool,
}

/// The closed-loop result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopOutcome {
    /// The central supply decision.
    pub decision: SupplyDecision,
    /// The requirement set actually evaluated (after relaxations).
    pub requirement_set: EngineeringRequirementSet,
    /// Relaxations applied in order.
    pub applied_relaxations: Vec<AppliedRelaxation>,
    /// Ranked candidates, best first.
    pub ranked: Vec<CandidateId>,
    /// Bounded candidate evaluations (best first).
    pub evaluations: Vec<CandidateEvaluation>,
    /// Knowledge authorities cited.
    pub knowledge: Vec<AuthorityRef>,
    /// 11-gate engineering response for the recommended candidate.
    pub response: Option<EngineeringResponse>,
    /// Executed analysis receipt.
    pub receipt: AnalysisReceipt,
    /// Independent SI cross-check receipt.
    pub cross_check: AnalysisReceipt,
    /// Negotiation outcome, when any candidate quoted.
    pub negotiation: Option<NegotiationOutcome>,
    /// Digest binding.
    pub binding: LoopBinding,
    /// Measured performance.
    pub performance: LoopPerformance,
    /// Authorization notice.
    pub authorization: AuthorizationNotice,
}

impl LoopOutcome {
    /// Recompute every proof/evidence binding. This is the command/tool-side
    /// verification entry point; it never trusts the stored booleans.
    pub fn verify(&self) -> LoopResult<LoopVerification> {
        self.verify_with_verifier(&ReceiptVerifier::self_attested())
    }

    /// Verify every proof binding and require the receipts (including the
    /// response gate receipts) to attest under the daemon's trusted key.
    pub fn verify_with_verifier(&self, verifier: &ReceiptVerifier) -> LoopResult<LoopVerification> {
        self.decision.validate()?;
        self.requirement_set.validate()?;
        verifier.verify(&self.receipt)?;
        verifier.verify(&self.cross_check)?;
        let payload = digest_payload(
            &self.decision,
            &self.requirement_set,
            &self.evaluations,
            &self.ranked,
            &self.knowledge,
        )?;
        let model_digest = Digest::of_json(&self.knowledge.to_vec())?;
        let selected_digest = match self.response.as_ref() {
            Some(response) => match &response.theory.state {
                engineering_core::GateState::Established(theory) => match &theory.selection {
                    Some(selection) => Some(selection.digest()?),
                    None => None,
                },
                _ => None,
            },
            None => None,
        };
        let selection_binding = match (&self.response, selected_digest) {
            (Some(_), Some(digest)) => {
                self.receipt.selection_digest == Some(digest)
                    && self.cross_check.selection_digest == Some(digest)
            }
            (Some(_), None) => false,
            (None, None) => true,
            (None, Some(_)) => false,
        };
        let receipt_binding = self.receipt.input_digest == self.binding.input_digest
            && self.cross_check.input_digest == self.binding.input_digest
            && self.receipt.model_digest == model_digest
            && self.cross_check.model_digest == model_digest
            && self.binding.engineering_revision == self.requirement_set.revision
            && self.binding.supply_revision == self.decision.revision
            && selection_binding;
        let response_ready = match (&self.response, &self.decision.recommended_action) {
            (Some(response), _) => response.is_ready_with_verifier(verifier),
            (None, RecommendedAction::Proceed { .. }) => false,
            (None, _) => true,
        };
        Ok(LoopVerification {
            decision_valid: true,
            requirement_set_valid: true,
            receipt_valid: receipt_binding,
            result_digest_matches: payload == self.binding.result_digest,
            response_ready,
            no_ungated_actions: self.authorization.executed_actions.is_empty()
                && self.authorization.orders_and_messages_require_grant,
        })
    }

    /// True when verification passed every binding.
    pub fn is_verified(&self) -> bool {
        self.is_verified_with_verifier(&ReceiptVerifier::self_attested())
    }

    pub fn is_verified_with_verifier(&self, verifier: &ReceiptVerifier) -> bool {
        self.verify_with_verifier(verifier)
            .map(|v| {
                v.decision_valid
                    && v.requirement_set_valid
                    && v.receipt_valid
                    && v.result_digest_matches
                    && v.response_ready
                    && v.no_ungated_actions
            })
            .unwrap_or(false)
    }
}

/// Run the closed loop. Deterministic and bounded; evaluation is O(n), and
/// ranking is one `sort_by` over n candidates (O(n log n)).
pub fn run(request: &LoopRequest) -> LoopResult<LoopOutcome> {
    // Standalone/embedded callers get an ephemeral process key: receipts are
    // still signed and self-verifiable, but the daemon path uses the durable
    // data-dir key through [`run_with_signer`].
    let signer = ReceiptSigner::generate();
    run_with_signer(request, &signer)
}

/// Run the closed loop with the daemon-local engineering signing key.
pub fn run_with_signer(request: &LoopRequest, signer: &ReceiptSigner) -> LoopResult<LoopOutcome> {
    request.validate()?;
    let started = Instant::now();
    let input_digest = Digest::of_json(request)?;

    let registry = KnowledgeRegistry::bundled()?;
    let mut packs: Vec<&KnowledgePack> = Vec::new();
    let mut seen_packs = BTreeSet::new();
    for domain in &request.knowledge_domains {
        let found = registry.by_domain(domain);
        if found.is_empty() {
            return Err(LoopError::Invalid(format!(
                "no bundled knowledge pack covers domain {domain:?}"
            )));
        }
        for pack in found {
            if seen_packs.insert((pack.id.clone(), pack.version.clone())) {
                packs.push(pack);
            }
        }
    }
    let authorities: Vec<AuthorityRef> = packs.iter().filter_map(|pack| pack.authority()).collect();
    if authorities.is_empty() {
        return Err(LoopError::Invalid(
            "knowledge authority could not be established".to_string(),
        ));
    }
    for model in &request.engineering.models {
        if let (Some(pack_id), Some(pack_version)) = (&model.pack_id, &model.pack_version) {
            if !packs
                .iter()
                .any(|pack| &pack.id == pack_id && &pack.version == pack_version)
            {
                return Err(LoopError::Invalid(format!(
                    "model {} cites pack {pack_id}@{pack_version}, which is not in the requested knowledge domains",
                    model.model_id
                )));
            }
        }
    }

    let baseline_verdicts = if request.relaxations.is_empty() {
        Vec::new()
    } else {
        request
            .candidates
            .iter()
            .map(|candidate| {
                evaluate_requirements(&request.engineering, &candidate.facts)
                    .map(|evaluation| evaluation.verdict)
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let mut requirements = request.engineering.clone();
    let mut applied_relaxations = Vec::new();
    for relaxation in &request.relaxations {
        let from_revision = requirements.revision;
        let from_limit = requirements
            .requirements
            .iter()
            .find(|requirement| requirement.id == relaxation.requirement_id)
            .and_then(|requirement| match &requirement.target {
                engineering_core::RequirementTarget::Numeric { limit, .. } => Some(limit.clone()),
                engineering_core::RequirementTarget::Categorical { .. } => None,
            })
            .ok_or_else(|| {
                LoopError::Invalid(format!(
                    "relaxation targets unknown numeric requirement {}",
                    relaxation.requirement_id
                ))
            })?;
        let next_revision = requirements.revision.next();
        let provenance = relaxation.provenance.clone().with_revision(next_revision);
        requirements = requirements.relax(
            &relaxation.requirement_id,
            relaxation.new_limit.clone(),
            &relaxation.rationale,
            provenance,
            &relaxation.falsifier,
            relaxation.evidence.clone(),
        )?;
        applied_relaxations.push(AppliedRelaxation {
            requirement_id: relaxation.requirement_id.clone(),
            from_revision,
            to_revision: requirements.revision,
            from_limit,
            to_limit: relaxation.new_limit.clone(),
            rationale: relaxation.rationale.clone(),
            evidence: relaxation.evidence.id.clone(),
        });
    }

    let package = rfq_package(request, &requirements)?;
    let normalization = NormalizationContext {
        target_quantity: request.requirement.quantity,
        cost_of_capital: request.negotiation.cost_of_capital,
        baseline_payment_days: request.negotiation.baseline_payment_days,
        as_of: Day(request.requirement.needed_by.0),
    };

    let mut evaluations = Vec::with_capacity(request.candidates.len());
    let mut quotes = Vec::new();
    for candidate in &request.candidates {
        evaluations.push(evaluate_candidate(
            request,
            &requirements,
            candidate,
            &package,
            &normalization,
            !applied_relaxations.is_empty(),
            &packs,
        )?);
        let quote = selected_quote(candidate, !applied_relaxations.is_empty());
        if let Some(quote) = quote {
            quotes.push(quote.clone());
        }
    }

    let mut order: Vec<usize> = (0..evaluations.len()).collect();
    order.sort_by(|left, right| {
        let left = &evaluations[*left];
        let right = &evaluations[*right];
        match (left.promotable, right.promotable) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        let key = |candidate: &CandidateEvaluation| {
            (
                candidate
                    .landed_per_unit
                    .as_known()
                    .map(|money| money.micros())
                    .unwrap_or(i64::MAX),
                candidate
                    .lead_time_days
                    .as_known()
                    .copied()
                    .unwrap_or(u32::MAX),
                candidate.candidate.0.clone(),
            )
        };
        key(left).cmp(&key(right))
    });

    let promotable = order
        .iter()
        .filter(|index| evaluations[**index].promotable)
        .count();
    let ranked: Vec<CandidateId> = order
        .iter()
        .filter(|index| evaluations[**index].promotable)
        .map(|index| evaluations[*index].candidate.clone())
        .collect();
    let recommended_index = order
        .iter()
        .copied()
        .find(|index| evaluations[*index].promotable);
    let top: Vec<usize> = order.iter().copied().take(MAX_LOOP_RANKED).collect();

    let DecisionParts {
        mut decision,
        response,
        receipt,
        cross_check,
        negotiation,
        evidence,
    } = build_decision(
        request,
        &requirements,
        &evaluations,
        recommended_index,
        &authorities,
        applied_relaxations.is_empty(),
        &baseline_verdicts,
        &applied_relaxations,
        &quotes,
        &package,
        &normalization,
        &packs,
        signer,
        started,
    )?;

    let mut returned = Vec::with_capacity(top.len());
    for index in &top {
        returned.push(evaluations[*index].clone());
    }
    if let Some(index) = recommended_index {
        if !top.contains(&index) {
            returned.push(evaluations[index].clone());
        }
    }

    let candidate_by_id = index_candidates(&request.candidates);
    decision.candidates = returned
        .iter()
        .map(|evaluation| {
            let source = candidate_by_id.get(evaluation.candidate.0.as_str());
            Candidate {
                id: evaluation.candidate.clone(),
                supplier: evaluation.supplier,
                role: evaluation.role,
                site: source.and_then(|candidate| candidate.site),
                evidence: source
                    .map(|candidate| candidate.evidence.clone())
                    .unwrap_or_default(),
            }
        })
        .take(MAX_CANDIDATES)
        .collect();
    if matches!(
        decision.recommended_action,
        RecommendedAction::Proceed { .. }
    ) && !decision
        .candidates
        .iter()
        .any(|candidate| Some(&candidate.id) == recommended_id(&decision))
    {
        if let Some(index) = recommended_index {
            decision.candidates.insert(
                0,
                Candidate {
                    id: evaluations[index].candidate.clone(),
                    supplier: evaluations[index].supplier,
                    role: evaluations[index].role,
                    site: request.candidates[index].site,
                    evidence: evaluations[index]
                        .supplier_fit
                        .as_ref()
                        .map(|fit| fit.evidence.clone())
                        .unwrap_or_default(),
                },
            );
            decision.candidates.truncate(MAX_CANDIDATES);
        }
    }
    decision.evidence = evidence;
    decision.validate()?;

    let result_digest = digest_payload(&decision, &requirements, &returned, &ranked, &authorities)?;
    let payload = LoopBinding {
        input_digest,
        result_digest,
        engineering_revision: requirements.revision,
        supply_revision: request.revision,
    };

    let elapsed_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    let outcome = LoopOutcome {
        decision,
        requirement_set: requirements,
        applied_relaxations,
        ranked,
        evaluations: returned,
        knowledge: authorities,
        response,
        receipt,
        cross_check,
        negotiation,
        binding: payload,
        performance: LoopPerformance {
            evaluated_candidates: evaluations.len(),
            promotable_candidates: promotable,
            returned_candidates: top.len(),
            elapsed_micros,
        },
        authorization: AuthorizationNotice {
            orders_and_messages_require_grant: true,
            executed_actions: Vec::new(),
        },
    };
    Ok(outcome)
}

fn index_candidates(candidates: &[LoopCandidate]) -> BTreeMap<&str, &LoopCandidate> {
    candidates
        .iter()
        .map(|candidate| (candidate.id.0.as_str(), candidate))
        .collect()
}

fn recommended_id(decision: &SupplyDecision) -> Option<&CandidateId> {
    match &decision.recommended_action {
        RecommendedAction::Proceed { candidate } => Some(candidate),
        _ => None,
    }
}

fn selected_quote(candidate: &LoopCandidate, relaxed: bool) -> Option<&Quote> {
    if relaxed {
        candidate
            .relaxed_quote
            .as_ref()
            .or(candidate.quote.as_ref())
    } else {
        candidate.quote.as_ref()
    }
}

fn selected_tco(candidate: &LoopCandidate, relaxed: bool) -> Option<&TcoInput> {
    if relaxed {
        candidate.relaxed_tco.as_ref().or(candidate.tco.as_ref())
    } else {
        candidate.tco.as_ref()
    }
}

fn selected_should_cost(candidate: &LoopCandidate, relaxed: bool) -> Option<&ShouldCostInput> {
    if relaxed {
        candidate
            .relaxed_should_cost
            .as_ref()
            .or(candidate.should_cost.as_ref())
    } else {
        candidate.should_cost.as_ref()
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_candidate(
    request: &LoopRequest,
    requirements: &EngineeringRequirementSet,
    candidate: &LoopCandidate,
    package: &RfqPackage,
    normalization: &NormalizationContext,
    relaxed: bool,
    packs: &[&KnowledgePack],
) -> LoopResult<CandidateEvaluation> {
    let mut engineering = evaluate_requirements(requirements, &candidate.facts)?;
    let knowledge_refs = augment_tests(requirements, &mut engineering, packs);

    let supplier_fit = match (&candidate.supplier_profile, &request.sourcing) {
        (Some(profile), Some(sourcing)) => Some(assess_supplier(profile, sourcing)?),
        _ => None,
    };

    let tco_input = selected_tco(candidate, relaxed);
    let tco_result = tco_input.map(tco).transpose()?;
    let should_cost_result = selected_should_cost(candidate, relaxed)
        .map(should_cost)
        .transpose()?;
    let quote = selected_quote(candidate, relaxed);
    let normalized_bid = quote
        .map(|quote| normalize_quote(quote, package, normalization))
        .transpose()?;

    let profile_lead_time = candidate
        .supplier_profile
        .as_ref()
        .and_then(|profile| profile.metrics.lead_time_days.as_known())
        .map(|distribution| distribution.p50.max(0) as u32);
    let lead_time_days = match candidate.lead_time_days.value.as_known().copied() {
        Some(days) => Availability::known(days),
        None => match profile_lead_time {
            Some(days) => Availability::known(days),
            None => Availability::unavailable(
                UnavailableReason::MissingEvidence,
                "no lead-time observation",
            ),
        },
    };

    let quality_risk = candidate
        .quality_risk
        .clone()
        .map(Availability::known)
        .unwrap_or_else(|| derive_quality_risk(candidate));
    let continuity_risk = candidate
        .continuity_risk
        .clone()
        .map(Availability::known)
        .unwrap_or_else(|| derive_continuity_risk(candidate));
    let lifecycle_risk = match &candidate.lifecycle_risk {
        Some(risk) => Availability::known(risk.clone()),
        None => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "no PCN/EOL observation for this part",
        ),
    };

    let landed_per_unit = tco_result
        .as_ref()
        .map(|value| value.per_unit.clone())
        .unwrap_or_else(|| {
            Availability::unavailable(UnavailableReason::MissingEvidence, "no TCO input")
        });
    let landed_total = tco_result
        .as_ref()
        .map(|value| value.total.clone())
        .unwrap_or_else(|| {
            Availability::unavailable(UnavailableReason::MissingEvidence, "no TCO input")
        });

    let mut blockers = Vec::new();
    if !engineering.verdict.promotable() {
        blockers.push(format!(
            "engineering verdict {}",
            engineering.verdict.as_str()
        ));
    }
    if request.require_supplier_fit {
        match &supplier_fit {
            Some(report)
                if matches!(
                    report.verdict,
                    FitVerdict::Fit | FitVerdict::FitWithConditions
                ) => {}
            Some(report) => blockers.push(format!("supplier verdict {:?}", report.verdict)),
            None => blockers.push("supplier fit not assessed".to_string()),
        }
    }
    if tco_result.is_none() || !landed_total.is_known() {
        blockers.push("landed TCO unavailable".to_string());
    }
    if !lead_time_days.is_known() {
        blockers.push("lead time unavailable".to_string());
    }

    Ok(CandidateEvaluation {
        candidate: candidate.id.clone(),
        supplier: candidate.supplier,
        role: candidate.role,
        engineering,
        supplier_fit: supplier_fit.as_ref().map(SupplierFitSummary::from_report),
        should_cost: should_cost_result,
        tco: tco_result,
        normalized_bid,
        landed_per_unit,
        landed_total,
        lead_time_days,
        quality_risk,
        continuity_risk,
        lifecycle_risk,
        promotable: blockers.is_empty(),
        blockers,
        derived_tests: Vec::new(),
        knowledge_refs,
    })
    .map(|mut evaluation| {
        evaluation.derived_tests = evaluation.engineering.tests.clone();
        evaluation
    })
}

fn augment_tests(
    requirements: &EngineeringRequirementSet,
    evaluation: &mut EngineeringCandidateEvaluation,
    packs: &[&KnowledgePack],
) -> Vec<String> {
    let mut refs = Vec::new();
    let property_by_requirement: BTreeMap<&str, &str> = requirements
        .requirements
        .iter()
        .map(|requirement| (requirement.id.as_str(), requirement.target.property()))
        .collect();
    for outcome in &evaluation.outcomes {
        if outcome.state.is_satisfied() {
            continue;
        }
        let Some(property) = property_by_requirement.get(outcome.requirement_id.as_str()) else {
            continue;
        };
        for pack in packs {
            for method in &pack.test_methods {
                if !method_mentions(method, property) {
                    continue;
                }
                let reference = format!("{}@{}#{}", pack.id, pack.version, method.id);
                if evaluation
                    .tests
                    .iter()
                    .any(|test| test.knowledge_ref.as_deref() == Some(reference.as_str()))
                {
                    continue;
                }
                if evaluation.tests.len() >= engineering_core::MAX_DERIVED_TESTS {
                    return refs;
                }
                evaluation.tests.push(DerivedTest {
                    requirement_id: outcome.requirement_id.clone(),
                    kind: match outcome.discipline {
                        Discipline::Reliability => engineering_core::TestKind::Reliability,
                        Discipline::Tolerance | Discipline::Manufacturing => {
                            engineering_core::TestKind::Inspection
                        }
                        _ => engineering_core::TestKind::Measurement,
                    },
                    claim: format!(
                        "{} resolves {} with {}",
                        method.method, outcome.requirement_id, property
                    ),
                    procedure: method.method.clone(),
                    pass_criterion: format!("measured {} meets the requirement limit", property),
                    refutation: format!(
                        "a measurement of {} that violates the requirement limit",
                        property
                    ),
                    instrument: method
                        .standard_ref
                        .clone()
                        .unwrap_or_else(|| "calibrated measurement system".to_string()),
                    knowledge_ref: Some(reference.clone()),
                    evidence: vec![outcome.requirement_id.clone(), pack.id.clone()],
                });
                refs.push(reference);
            }
        }
    }
    refs.sort();
    refs.dedup();
    refs
}

fn method_mentions(method: &engineering_knowledge::TestMethod, property: &str) -> bool {
    let property = property.to_ascii_lowercase();
    let aliases: &[&str] = if property.contains("esr") {
        &["esr", "impedance"]
    } else if property.contains("ripple") {
        &["ripple"]
    } else if property.contains("dielectric") {
        &["dielectric"]
    } else if property.contains("cap") {
        &["capacitance", "dielectric", "impedance"]
    } else if property.contains("size")
        || property.contains("diameter")
        || property.contains("width")
    {
        &["dimension", "case"]
    } else {
        &[property.as_str()]
    };
    let haystack = format!("{} {}", method.method, method.measures).to_ascii_lowercase();
    aliases.iter().any(|alias| haystack.contains(alias))
}

fn derive_quality_risk(candidate: &LoopCandidate) -> Availability<RiskSummary> {
    let Some(profile) = &candidate.supplier_profile else {
        return Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "no supplier quality observations",
        );
    };
    let Some(ppm) = profile.metrics.defect_ppm.as_known() else {
        return Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "defect-rate distribution unavailable",
        );
    };
    let level = if ppm.p90 <= 500 {
        RiskLevel::Low
    } else if ppm.p90 <= 5_000 {
        RiskLevel::Medium
    } else if ppm.p90 <= 50_000 {
        RiskLevel::High
    } else {
        RiskLevel::Critical
    };
    let mut evidence = Vec::new();
    if let Some(basis) = &profile.metrics.defect_ppm.basis {
        collect_basis_evidence(basis, &mut evidence);
    }
    Availability::known(RiskSummary {
        level,
        drivers: vec![format!("defect rate P90 = {} ppm", ppm.p90)],
        evidence,
    })
}

fn derive_continuity_risk(candidate: &LoopCandidate) -> Availability<RiskSummary> {
    let Some(profile) = &candidate.supplier_profile else {
        return Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "no supplier continuity evidence",
        );
    };
    let mut level = None;
    let mut drivers = Vec::new();
    let mut evidence = Vec::new();
    for exposure in &profile.continuity {
        if let Some(value) = exposure.as_known() {
            level = Some(level.map_or(value.severity, |current: RiskLevel| {
                current.max(value.severity)
            }));
            drivers.push(format!(
                "{:?}: recovery {} days",
                value.kind,
                value.recovery_days.value.as_known().copied().unwrap_or(0)
            ));
            if let Some(basis) = &exposure.basis {
                collect_basis_evidence(basis, &mut evidence);
            }
        }
    }
    match level {
        Some(level) => Availability::known(RiskSummary {
            level,
            drivers,
            evidence,
        }),
        None => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "no continuity exposure assessed",
        ),
    }
}

fn rfq_package(
    request: &LoopRequest,
    requirements: &EngineeringRequirementSet,
) -> LoopResult<RfqPackage> {
    let evidence = request
        .evidence
        .first()
        .cloned()
        .or_else(|| {
            requirements
                .evidence
                .first()
                .map(engineering_evidence_to_supply)
        })
        .unwrap_or(EvidenceRef {
            id: requirements.id.clone(),
            revision: Revision(requirements.revision.get()),
            digest: None,
        });
    let package = RfqPackage {
        id: format!("{}-rfq", request.decision_id),
        revision: request.revision,
        part: request.requirement.part.unwrap_or(PartId(0)),
        description: request.requirement.description.clone(),
        currency: request.requirement.currency,
        volume_breaks: vec![request.requirement.quantity],
        required_processes: Vec::<ProcessKind>::new(),
        required_qualifications: Vec::new(),
        incoterm: request.negotiation.requested_incoterm,
        destination_country: request.negotiation.destination_country.clone(),
        payment_days: request.negotiation.baseline_payment_days,
        required_by: request.requirement.needed_by,
        should_cost_target: None,
        questions: vec![],
        requirement_evidence: evidence,
    };
    package.validate()?;
    Ok(package)
}

struct DecisionParts {
    decision: SupplyDecision,
    response: Option<EngineeringResponse>,
    receipt: AnalysisReceipt,
    cross_check: AnalysisReceipt,
    negotiation: Option<NegotiationOutcome>,
    evidence: Vec<EvidenceRef>,
}

#[allow(clippy::too_many_arguments)]
fn build_decision(
    request: &LoopRequest,
    requirements: &EngineeringRequirementSet,
    evaluations: &[CandidateEvaluation],
    recommended_index: Option<usize>,
    authorities: &[AuthorityRef],
    no_relaxation_applied: bool,
    baseline_verdicts: &[EngineeringVerdict],
    relaxations: &[AppliedRelaxation],
    quotes: &[Quote],
    package: &RfqPackage,
    normalization: &NormalizationContext,
    packs: &[&KnowledgePack],
    signer: &ReceiptSigner,
    started: Instant,
) -> LoopResult<DecisionParts> {
    let mut evidence = Vec::new();
    for reference in &request.evidence {
        push_evidence(&mut evidence, reference.clone());
    }
    for item in &requirements.evidence {
        push_evidence(&mut evidence, engineering_evidence_to_supply(item));
    }
    for candidate in &request.candidates {
        for reference in &candidate.evidence {
            push_evidence(&mut evidence, reference.clone());
        }
        if let Some(quote) = selected_quote(candidate, !no_relaxation_applied) {
            push_evidence(&mut evidence, quote.document.clone());
        }
    }
    for evaluation in evaluations {
        if let Some(fit) = &evaluation.supplier_fit {
            for reference in &fit.evidence {
                push_evidence(&mut evidence, reference.clone());
            }
        }
        if let Some(tco) = &evaluation.tco {
            for line in &tco.lines {
                for basis in &line.basis {
                    collect_basis_evidence(basis, &mut evidence);
                }
            }
        }
        if let Some(cost) = &evaluation.should_cost {
            for line in &cost.lines {
                for basis in &line.basis {
                    collect_basis_evidence(basis, &mut evidence);
                }
            }
        }
    }
    if evidence.is_empty() {
        return Err(LoopError::Invalid(
            "the decision needs at least one evidence reference".to_string(),
        ));
    }

    let candidate_by_id = index_candidates(&request.candidates);
    let mut negotiation = None;
    if !quotes.is_empty() {
        let comparison = compare_bids(quotes, package, normalization)?;
        let batna = batna(&comparison);
        let negotiation_plan = negotiation_plan(package, &comparison);
        negotiation = Some(NegotiationOutcome {
            package: package.clone(),
            comparison,
            batna,
            negotiation: negotiation_plan,
        });
    }

    let recommended = recommended_index.map(|index| &evaluations[index]);
    let mut assumptions = request.assumptions.clone();
    if let Some(recommended) = recommended {
        for candidate in &request.candidates {
            if candidate.id == recommended.candidate {
                for line in recommended
                    .tco
                    .as_ref()
                    .map(|tco| tco.lines.as_slice())
                    .unwrap_or(&[])
                {
                    for basis in &line.basis {
                        collect_basis_assumption(basis, &mut assumptions);
                    }
                }
                for line in recommended
                    .should_cost
                    .as_ref()
                    .map(|cost| cost.lines.as_slice())
                    .unwrap_or(&[])
                {
                    for basis in &line.basis {
                        collect_basis_assumption(basis, &mut assumptions);
                    }
                }
            }
        }
    }
    dedup_assumptions(&mut assumptions)?;

    let mut uncertainty = Vec::new();
    for evaluation in evaluations.iter().take(MAX_LOOP_RANKED) {
        for outcome in &evaluation.engineering.outcomes {
            if outcome.state.is_satisfied() {
                continue;
            }
            push_uncertainty(
                &mut uncertainty,
                UncertaintyNote {
                    subject: format!("{}:{}", evaluation.candidate.0, outcome.requirement_id),
                    class: match &outcome.state {
                        OutcomeState::Violated { .. }
                        | OutcomeState::CategoricalMismatch { .. }
                        | OutcomeState::DimensionMismatch { .. } => UncertaintyClass::Exact,
                        _ => UncertaintyClass::Unknown,
                    },
                    note: outcome_note(&outcome.state),
                },
            );
        }
        if let Some(tco) = &evaluation.tco {
            for term in &tco.unavailable_terms {
                push_uncertainty(
                    &mut uncertainty,
                    UncertaintyNote {
                        subject: format!("{}:{term:?}", evaluation.candidate.0),
                        class: UncertaintyClass::Unknown,
                        note: format!("TCO term {term:?} is unavailable; the total is not substituted with zero"),
                    },
                );
            }
        }
        if request.require_supplier_fit && evaluation.supplier_fit.is_none() {
            push_uncertainty(
                &mut uncertainty,
                UncertaintyNote {
                    subject: format!("{}:supplier", evaluation.candidate.0),
                    class: UncertaintyClass::Unknown,
                    note: "supplier capability was not assessed".to_string(),
                },
            );
        }
    }

    let mut counterfactuals = Vec::new();
    if let Some(recommended) = recommended {
        for alternative in evaluations
            .iter()
            .filter(|evaluation| {
                evaluation.promotable && evaluation.candidate != recommended.candidate
            })
            .take(6)
        {
            counterfactuals.push(Counterfactual {
                description: format!(
                    "choose {} instead of {}",
                    alternative.candidate.0, recommended.candidate.0
                ),
                outcome: format!(
                    "landed total {}; delta {}; engineering {}; lead time {} days",
                    money_text(&alternative.landed_total),
                    delta_text(&alternative.landed_total, &recommended.landed_total),
                    alternative.engineering.verdict.as_str(),
                    alternative
                        .lead_time_days
                        .as_known()
                        .map(|days| days.to_string())
                        .unwrap_or_else(|| "unavailable".to_string())
                ),
                evidence: alternative
                    .engineering
                    .outcomes
                    .iter()
                    .map(|outcome| EvidenceRef {
                        id: format!("{}:{}", alternative.candidate.0, outcome.requirement_id),
                        revision: Revision(
                            outcome.provenance.revision.map(|r| r.get()).unwrap_or(1),
                        ),
                        digest: outcome.provenance.digest.map(|digest| digest.to_hex()),
                    })
                    .take(4)
                    .collect(),
            });
            if counterfactuals.len() >= MAX_COUNTERFACTUALS {
                break;
            }
        }
        let unit_price_leader = evaluations
            .iter()
            .filter_map(|evaluation| {
                let quote = candidate_by_id
                    .get(evaluation.candidate.0.as_str())
                    .and_then(|candidate| selected_quote(candidate, !no_relaxation_applied))?;
                quoted_unit_price(quote, request.requirement.quantity)
                    .as_known()
                    .copied()
                    .map(|price| (price, evaluation))
            })
            .min_by(|left, right| {
                left.0
                    .micros()
                    .cmp(&right.0.micros())
                    .then(left.1.candidate.0.cmp(&right.1.candidate.0))
            });
        if let Some((_price, leader)) = unit_price_leader {
            if leader.candidate != recommended.candidate {
                counterfactuals.push(Counterfactual {
                    description: format!(
                        "procure at the lowest quoted unit price ({})",
                        leader.candidate.0
                    ),
                    outcome: format!(
                        "landed total {}; delta {}; unit-price saving does not survive MOQ, freight, quality and cash terms",
                        money_text(&leader.landed_total),
                        delta_text(&leader.landed_total, &recommended.landed_total)
                    ),
                    evidence: leader
                        .supplier_fit
                        .as_ref()
                        .map(|fit| fit.evidence.clone())
                        .unwrap_or_default()
                        .into_iter()
                        .take(4)
                        .collect(),
                });
            }
        }
    }
    for relaxation in relaxations {
        let baseline = baseline_verdicts
            .iter()
            .map(|verdict| verdict.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        counterfactuals.push(Counterfactual {
            description: format!(
                "keep {} at the pre-relaxation limit ({})",
                relaxation.requirement_id,
                relaxation.from_limit.describe()
            ),
            outcome: format!(
                "baseline revision {} verdicts: [{}]; relaxation to {} uses evidence {} and the revised quote/should-cost when supplied",
                relaxation.from_revision,
                baseline,
                relaxation.to_limit.describe(),
                relaxation.evidence
            ),
            evidence: vec![EvidenceRef {
                id: relaxation.evidence.clone(),
                revision: Revision(relaxation.to_revision.get()),
                digest: None,
            }],
        });
        if counterfactuals.len() >= MAX_COUNTERFACTUALS {
            break;
        }
    }

    let mut conflicts = Vec::new();
    if let Some(recommended) = recommended {
        if let Some(candidate) = request
            .candidates
            .iter()
            .find(|candidate| candidate.id == recommended.candidate)
        {
            if let (Some(profile), Some(sourcing)) =
                (&candidate.supplier_profile, &request.sourcing)
            {
                let report = assess_supplier(profile, sourcing)?;
                for conflict in report.conflicts.iter().take(MAX_CONFLICTS) {
                    conflicts.push(conflict.clone());
                }
            }
        }
    }

    let response = match recommended {
        Some(evaluation) if evaluation.promotable => Some(build_response(
            request,
            requirements,
            evaluation,
            authorities,
            &evidence,
            &assumptions,
            packs,
            signer,
        )?),
        _ => None,
    };
    let recommended_action = match (response.as_ref(), recommended) {
        (Some(response), Some(evaluation)) => {
            if response.is_ready() {
                RecommendedAction::Proceed {
                    candidate: evaluation.candidate.clone(),
                }
            } else {
                let assessment = response.assess();
                let missing = assessment
                    .violations
                    .iter()
                    .map(|violation| format!("gate {}", violation.gate))
                    .chain(
                        assessment
                            .research_required
                            .iter()
                            .map(|request| format!("research {}", request.gate)),
                    )
                    .collect::<Vec<_>>();
                RecommendedAction::RequestEvidence {
                    missing: bounded_missing(missing),
                }
            }
        }
        _ => {
            let mut missing = BTreeSet::new();
            for evaluation in evaluations {
                if evaluation.promotable {
                    continue;
                }
                for blocker in &evaluation.blockers {
                    missing.insert(format!("{}: {blocker}", evaluation.candidate.0));
                }
            }
            if missing.is_empty() {
                RecommendedAction::Refuse {
                    reason: "no candidate satisfies the mandatory engineering and supply gates"
                        .to_string(),
                }
            } else {
                RecommendedAction::RequestEvidence {
                    missing: bounded_missing(missing.into_iter().collect()),
                }
            }
        }
    };

    let technical_fit = match recommended {
        Some(evaluation) => Availability::known(fit_assessment(evaluation)),
        None => Availability::unavailable(
            UnavailableReason::NoCandidate,
            "no candidate cleared the engineering gates",
        ),
    };
    let supplier_capability =
        match recommended.and_then(|evaluation| evaluation.supplier_fit.as_ref()) {
            Some(fit) if fit.promotable() => Availability::known(CapabilitySummary {
                role: recommended
                    .map(|evaluation| evaluation.role)
                    .unwrap_or(SupplierRole::Manufacturer),
                processes: recommended
                    .and_then(|evaluation| {
                        request
                            .candidates
                            .iter()
                            .find(|candidate| candidate.id == evaluation.candidate)
                            .map(|candidate| candidate.processes.clone())
                    })
                    .unwrap_or_default(),
                qualifications: Vec::new(),
                evidence: fit.evidence.clone(),
            }),
            Some(_) => Availability::unavailable(
                UnavailableReason::OutOfRange,
                "supplier fit is not promotable",
            ),
            None => Availability::unavailable(
                UnavailableReason::MissingEvidence,
                "supplier capability not assessed",
            ),
        };
    let commercial_terms = match recommended {
        Some(evaluation) => commercial_terms(request, evaluation, no_relaxation_applied),
        None => Availability::unavailable(
            UnavailableReason::NoCandidate,
            "no quoted candidate was promoted",
        ),
    };
    let landed_cost = match recommended {
        Some(evaluation) => match &evaluation.tco {
            Some(value) => Availability::known(LandedCostSummary {
                per_unit: value.per_unit.clone(),
                total: value.total.clone(),
                missing_terms: value
                    .unavailable_terms
                    .iter()
                    .map(|term| format!("{term:?}"))
                    .collect(),
                evidence: evaluation
                    .supplier_fit
                    .as_ref()
                    .map(|fit| fit.evidence.clone())
                    .unwrap_or_default(),
            }),
            None => Availability::unavailable(
                UnavailableReason::MissingEvidence,
                "no landed TCO for the promoted candidate",
            ),
        },
        None => Availability::unavailable(UnavailableReason::NoCandidate, "no promoted candidate"),
    };
    let quality_risk = recommended
        .map(|evaluation| evaluation.quality_risk.clone())
        .unwrap_or_else(|| {
            Availability::unavailable(UnavailableReason::NoCandidate, "no promoted candidate")
        });
    let continuity_risk = recommended
        .map(|evaluation| evaluation.continuity_risk.clone())
        .unwrap_or_else(|| {
            Availability::unavailable(UnavailableReason::NoCandidate, "no promoted candidate")
        });
    let lifecycle_risk = recommended
        .map(|evaluation| evaluation.lifecycle_risk.clone())
        .unwrap_or_else(|| {
            Availability::unavailable(UnavailableReason::NoCandidate, "no promoted candidate")
        });
    let logistics_plan = match recommended {
        Some(evaluation) => {
            let quote = request
                .candidates
                .iter()
                .find(|candidate| candidate.id == evaluation.candidate)
                .and_then(|candidate| selected_quote(candidate, !no_relaxation_applied));
            Availability::known(LogisticsSummary {
                mode: quote
                    .map(|quote| quote.mode)
                    .unwrap_or(request.negotiation.transport_mode),
                incoterm: quote
                    .map(|quote| quote.incoterm)
                    .unwrap_or(request.negotiation.requested_incoterm),
                transit_days: request
                    .candidates
                    .iter()
                    .find(|candidate| candidate.id == evaluation.candidate)
                    .map(|candidate| candidate.transit_days.clone())
                    .unwrap_or_else(|| {
                        Availability::unavailable(
                            UnavailableReason::MissingEvidence,
                            "no transit observation",
                        )
                    }),
                chargeable_weight_g: request
                    .candidates
                    .iter()
                    .find(|candidate| candidate.id == evaluation.candidate)
                    .map(|candidate| candidate.chargeable_weight_g.clone())
                    .unwrap_or_else(|| {
                        Availability::unavailable(
                            UnavailableReason::MissingEvidence,
                            "no chargeable weight",
                        )
                    }),
                evidence: quote
                    .map(|quote| vec![quote.document.clone()])
                    .unwrap_or_default(),
            })
        }
        None => Availability::unavailable(UnavailableReason::NoCandidate, "no promoted candidate"),
    };
    let inventory_effect = inventory_summary(request)?;

    let decision = SupplyDecision {
        decision_id: request.decision_id.clone(),
        revision: request.revision,
        requirement: request.requirement.clone(),
        candidates: Vec::new(),
        technical_fit,
        supplier_capability,
        commercial_terms,
        landed_cost,
        quality_risk,
        continuity_risk,
        logistics_plan,
        inventory_effect: Availability::known(inventory_effect),
        lifecycle_risk,
        assumptions,
        uncertainty,
        evidence: evidence.clone(),
        counterfactuals,
        recommended_action,
        conflicts,
    };

    let selection_digest = match response.as_ref() {
        Some(response) => match &response.theory.state {
            engineering_core::GateState::Established(theory) => match &theory.selection {
                Some(selection) => Some(selection.digest()?),
                None => None,
            },
            _ => None,
        },
        None => None,
    };
    let (receipt, cross_check) = build_receipts(
        request,
        requirements,
        evaluations,
        authorities,
        &evidence,
        selection_digest,
        signer,
        started,
    )?;

    Ok(DecisionParts {
        decision,
        response,
        receipt,
        cross_check,
        negotiation,
        evidence,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_receipts(
    request: &LoopRequest,
    requirements: &EngineeringRequirementSet,
    evaluations: &[CandidateEvaluation],
    authorities: &[AuthorityRef],
    evidence: &[EvidenceRef],
    selection_digest: Option<Digest>,
    signer: &ReceiptSigner,
    started: Instant,
) -> LoopResult<(AnalysisReceipt, AnalysisReceipt)> {
    let input_digest = Digest::of_json(request)?;
    let model_digest = Digest::of_json(&authorities.to_vec())?;
    let settings_digest = Digest::of_json(&serde_json::json!({
        "max_candidates": MAX_LOOP_CANDIDATES,
        "max_ranked": MAX_LOOP_RANKED,
    }))?;
    let boundary = EngineeringValue::known(
        Quantity::dimensionless(request.revision.get() as f64)?,
        engineering_provenance_from_evidence(
            evidence.first().cloned().unwrap_or(EvidenceRef {
                id: request.decision_id.clone(),
                revision: request.revision,
                digest: None,
            }),
            ProvenanceKind::Calculation,
        ),
        requirements.revision,
    )?;

    let mut content = ReceiptContent::new(input_digest, model_digest, settings_digest)
        .with_boundary_condition("supply_revision", boundary);
    content.selection_digest = selection_digest;
    let mut results = Vec::new();
    for evaluation in evaluations.iter().take(MAX_LOOP_RANKED) {
        results.push(ReceiptResult {
            name: format!("candidate:{}", evaluation.candidate.0),
            value: ReceiptValue::Text(format!(
                "{}:{}:tests={}",
                evaluation.engineering.verdict.as_str(),
                if evaluation.promotable {
                    "promotable"
                } else {
                    "blocked"
                },
                evaluation.derived_tests.len()
            )),
            uncertainty: Uncertainty::Exact,
        });
    }
    content.results = results;
    content.property_revisions = requirements
        .requirements
        .iter()
        .take(MAX_LOOP_RANKED)
        .map(|requirement| PropertyRevision::new(requirement.id.clone(), requirements.revision))
        .collect::<Result<Vec<_>, _>>()?;
    let runtime_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let mut receipt = AnalysisReceipt::kernel(
        AnalysisIdentity {
            solver_id: "faktor-supply-engineering.loop".to_string(),
            solver_version: env!("CARGO_PKG_VERSION").to_string(),
            kind: AnalysisKind::InProcessKernel,
        },
        content.clone(),
        runtime_ms,
    )?;
    signer.sign_receipt(&mut receipt)?;

    let candidate_by_id = index_candidates(&request.candidates);
    let mut cross_results = Vec::new();
    let mut disagreements = 0usize;
    for evaluation in evaluations.iter().take(MAX_LOOP_RANKED) {
        let candidate = candidate_by_id
            .get(evaluation.candidate.0.as_str())
            .copied()
            .ok_or_else(|| {
                LoopError::Invalid(format!(
                    "candidate {} vanished during receipt construction",
                    evaluation.candidate.0
                ))
            })?;
        let verdict = si_crosscheck_verdict(requirements, &candidate.facts)?;
        if verdict != evaluation.engineering.verdict {
            disagreements += 1;
        }
        cross_results.push(ReceiptResult {
            name: format!("candidate:{}", evaluation.candidate.0),
            value: ReceiptValue::Text(verdict.as_str().to_string()),
            uncertainty: Uncertainty::Exact,
        });
    }
    let mut cross_content = content;
    cross_content.results = cross_results;
    let mut cross_check = AnalysisReceipt::kernel(
        AnalysisIdentity {
            solver_id: "faktor-supply-engineering.si-crosscheck".to_string(),
            solver_version: env!("CARGO_PKG_VERSION").to_string(),
            kind: AnalysisKind::InProcessKernel,
        },
        cross_content,
        runtime_ms,
    )?;
    signer.sign_receipt(&mut cross_check)?;
    if disagreements > 0 {
        return Err(LoopError::Invalid(format!(
            "{disagreements} SI cross-check disagreements; the analysis is not accepted"
        )));
    }
    Ok((receipt, cross_check))
}

fn build_applicability_selection(
    requirements: &EngineeringRequirementSet,
    candidate: &CandidateFacts,
    packs: &[&KnowledgePack],
) -> LoopResult<ApplicabilitySelection> {
    let catalog = build_model_catalog(requirements, packs)?;
    let requests = selection_requests(requirements, candidate);
    Ok(ModelSelector::select(&catalog, &requests)?)
}

fn build_model_catalog(
    requirements: &EngineeringRequirementSet,
    packs: &[&KnowledgePack],
) -> LoopResult<ModelCatalog> {
    let mut models = Vec::with_capacity(requirements.models.len());
    let mut seen = BTreeSet::new();
    for binding in &requirements.models {
        if !seen.insert(binding.model_id.clone()) {
            return Err(LoopError::Invalid(format!(
                "duplicate model binding {}",
                binding.model_id
            )));
        }
        let (disciplines, properties, domains, source_model_id, provenance) = match (
            &binding.pack_id,
            &binding.pack_version,
        ) {
            (Some(pack_id), Some(pack_version)) => {
                let pack = packs
                        .iter()
                        .find(|pack| &pack.id == pack_id && &pack.version == pack_version)
                        .ok_or_else(|| {
                            LoopError::Invalid(format!(
                                "model {} cites pack {pack_id}@{pack_version}, which is not in the requested knowledge domains",
                                binding.model_id
                            ))
                        })?;
                // A registered pack with one model may be bound under a
                // caller alias (legacy fixture ids); the validity domains
                // are still the registered ones, never the binding's.
                let source = pack
                        .models
                        .iter()
                        .find(|model| model.id == binding.model_id)
                        .or_else(|| {
                            (pack.models.len() == 1).then(|| pack.models.first()).flatten()
                        })
                        .ok_or_else(|| {
                            LoopError::Invalid(format!(
                                "model {} is not registered in pack {pack_id}@{pack_version} and the pack has no unique model to alias",
                                binding.model_id
                            ))
                        })?;
                if !binding
                    .equation_ids
                    .iter()
                    .all(|id| source.equation_ids.contains(id))
                {
                    return Err(LoopError::Invalid(format!(
                        "model {} cites an equation that is not registered for pack model {}",
                        binding.model_id, source.id
                    )));
                }
                let converted = pack
                    .model_catalog()?
                    .into_iter()
                    .find(|model| model.model_id == source.id)
                    .ok_or_else(|| {
                        LoopError::Invalid(format!(
                            "pack model {} vanished during catalog conversion",
                            source.id
                        ))
                    })?;
                (
                    converted.disciplines,
                    converted.properties,
                    converted.domains,
                    Some(source.id.clone()),
                    converted.provenance,
                )
            }
            (None, None) => {
                let mut disciplines = BTreeSet::new();
                let mut properties = property_tokens(&binding.model_id);
                for requirement in &requirements.requirements {
                    if requirement.target.model() == Some(binding.model_id.as_str()) {
                        disciplines.insert(requirement.discipline);
                        properties.extend(property_tokens(requirement.target.property()));
                    }
                }
                for equation in &binding.equation_ids {
                    properties.extend(property_tokens(equation));
                }
                properties.sort();
                properties.dedup();
                if disciplines.is_empty() {
                    disciplines.insert(Discipline::Other);
                }
                let provenance = SelectionProvenance {
                    kind: binding.provenance.kind.as_str().to_string(),
                    citation: binding.provenance.citation.clone(),
                    edition: None,
                    locator: binding.provenance.locator.clone(),
                    note: None,
                    digest: binding.provenance.digest,
                };
                let domain = ValidityDomain {
                    id: format!("explicit-{}", binding.model_id),
                    statement:
                        "explicit revision-bound model binding with its declared validity envelope"
                            .to_string(),
                    temperature: binding.temperature,
                    conditions: binding.conditions.clone(),
                    exclusions: binding.exclusions.clone(),
                    materials: Vec::new(),
                    provenance: provenance.clone(),
                };
                (
                    disciplines.into_iter().collect(),
                    properties,
                    vec![domain],
                    None,
                    provenance,
                )
            }
            _ => {
                return Err(LoopError::Invalid(
                    "pack id and version must be provided together".to_string(),
                ))
            }
        };
        models.push(CatalogModel {
            model_id: binding.model_id.clone(),
            source_model_id,
            version: binding.version.clone(),
            pack_id: binding.pack_id.clone(),
            pack_version: binding.pack_version.clone(),
            pack_digest: binding.pack_id.as_ref().and_then(|_| {
                packs
                    .iter()
                    .find(|pack| {
                        Some(&pack.id) == binding.pack_id.as_ref()
                            && Some(&pack.version) == binding.pack_version.as_ref()
                    })
                    .and_then(|pack| pack.digest())
            }),
            equation_ids: binding.equation_ids.clone(),
            disciplines,
            properties,
            domains,
            provenance,
        });
    }
    ModelCatalog::new(models).map_err(LoopError::Engineering)
}

fn selection_requests(
    requirements: &EngineeringRequirementSet,
    candidate: &CandidateFacts,
) -> Vec<SelectionRequest> {
    let mut requests = Vec::new();
    for requirement in &requirements.requirements {
        if requirement.target.model().is_none() {
            continue;
        }
        let property = requirement.target.property();
        let mut conditions = candidate.conditions.clone();
        for condition in &requirement.conditions {
            if !conditions.iter().any(|existing| existing == condition) {
                conditions.push(condition.clone());
            }
        }
        let value = candidate.values.get(property);
        if let Some(value) = value {
            if let Some(condition) = &value.condition {
                if !conditions.iter().any(|existing| existing == condition) {
                    conditions.push(condition.clone());
                }
            }
            for condition in &value.applicability.conditions {
                if !conditions.iter().any(|existing| existing == condition) {
                    conditions.push(condition.clone());
                }
            }
        }
        let material = value
            .and_then(|value| value.material.as_ref())
            .map(|material| {
                let mut text = material.name.clone();
                if let Some(grade) = &material.grade {
                    text.push(' ');
                    text.push_str(grade);
                }
                if let Some(temper) = &material.temper {
                    text.push(' ');
                    text.push_str(temper);
                }
                text
            });
        requests.push(SelectionRequest {
            requirement_id: requirement.id.clone(),
            discipline: requirement.discipline,
            property: Some(property.to_string()),
            conditions,
            material,
            temperature: candidate.temperature.clone(),
        });
    }
    requests
}

fn property_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn selection_research(selection: &ApplicabilitySelection) -> (String, Vec<String>) {
    for outcome in &selection.selections {
        match outcome {
            RequirementSelection::Selected { .. } => {}
            RequirementSelection::ResearchRequired {
                requirement_id,
                reason,
                sources_required,
            } => {
                return (
                    format!("{requirement_id}: {reason}"),
                    sources_required.clone(),
                )
            }
            RequirementSelection::Ambiguous {
                requirement_id,
                candidates,
                reason,
            } => {
                return (
                    format!("{requirement_id}: {reason}"),
                    candidates
                        .iter()
                        .map(|candidate| format!("disambiguate {candidate}"))
                        .collect(),
                )
            }
        }
    }
    (
        "applicability selector returned no selected model".to_string(),
        vec!["a registered model with an established validity domain".to_string()],
    )
}

#[allow(clippy::too_many_arguments)]
fn build_response(
    request: &LoopRequest,
    requirements: &EngineeringRequirementSet,
    evaluation: &CandidateEvaluation,
    authorities: &[AuthorityRef],
    evidence: &[EvidenceRef],
    assumptions: &[Assumption],
    packs: &[&KnowledgePack],
    signer: &ReceiptSigner,
) -> LoopResult<EngineeringResponse> {
    let candidate = request
        .candidates
        .iter()
        .find(|candidate| candidate.id == evaluation.candidate)
        .ok_or_else(|| LoopError::Invalid("recommended candidate is missing".to_string()))?;
    let node = candidate.facts.node.unwrap_or_else(|| NodeId::new(1));
    let evidence_ids: Vec<String> = evidence
        .iter()
        .take(16)
        .map(|reference| reference.id.clone())
        .collect();
    let mut unknowns = Vec::new();
    let mut dimension_checks = Vec::new();
    let mut uncertainty_contributions = Vec::new();
    for outcome in &evaluation.engineering.outcomes {
        if outcome.state.is_satisfied() {
            uncertainty_contributions.push(UncertaintyContribution {
                source: outcome.requirement_id.clone(),
                uncertainty: Uncertainty::Exact,
                note: "typed value compared within its dimensioned unit".to_string(),
            });
        } else {
            unknowns.push(format!(
                "{}:{}",
                outcome.requirement_id,
                outcome_note(&outcome.state)
            ));
        }
    }
    if uncertainty_contributions.is_empty() {
        uncertainty_contributions.push(UncertaintyContribution {
            source: "requirement-set".to_string(),
            uncertainty: Uncertainty::Exact,
            note:
                "no requirement had an exact comparison; unresolved claims are listed as unknowns"
                    .to_string(),
        });
    }
    dimension_checks.extend(evaluation.engineering.dimension_checks.clone());
    if dimension_checks.is_empty() {
        return Err(LoopError::Invalid(
            "the response has no machine-checked dimensional comparison".to_string(),
        ));
    }

    let formulation = Formulation {
        objective: requirements.objective.clone(),
        scope: format!(
            "{}; candidate {} at revision {}",
            requirements.scope, candidate.id.0, candidate.facts.revision
        ),
        requirements: vec![node],
        constraints: requirements
            .constraints
            .iter()
            .map(|_| node)
            .collect::<Vec<_>>(),
        assumptions: assumptions
            .iter()
            .take(16)
            .map(|assumption| AssumptionRecord {
                id: assumption.id.clone(),
                statement: assumption.statement.clone(),
                provenance: engineering_provenance_from_supply(&assumption.provenance),
            })
            .collect(),
        unknowns,
    };
    let knowledge = KnowledgeAuthority {
        packs: authorities.to_vec(),
    };
    let selection = build_applicability_selection(requirements, &candidate.facts, packs)?;
    let (theory, validity) = if selection.is_all_selected() {
        let models = selection
            .unique_models()
            .into_iter()
            .map(|model| ModelSelection {
                model_id: model.model_id.clone(),
                version: model.version.clone(),
                equation_ids: model.equation_ids.clone(),
                rationale: model.rationale.clone(),
            })
            .collect::<Vec<_>>();
        let mut alternatives = Vec::new();
        for outcome in &selection.selections {
            if let RequirementSelection::Selected {
                alternatives_considered,
                ..
            } = outcome
            {
                alternatives.extend(alternatives_considered.iter().cloned());
            }
        }
        alternatives.sort();
        alternatives.dedup();
        if alternatives.is_empty() {
            alternatives.push(
                "no registered alternative model covers these requirement regimes".to_string(),
            );
        }
        let theory = TheorySelection {
            models,
            alternatives_considered: alternatives,
            selection: Some(selection.clone()),
        };
        let validity = ValidityEnvelopes {
            envelopes: selection.envelopes()?,
        };
        (Gate::established(theory), Gate::established(validity))
    } else {
        let (reason, sources) = selection_research(&selection);
        (
            Gate::research_required(GateId::Theory, reason.clone(), sources.clone()),
            Gate::research_required(GateId::Validity, reason, sources),
        )
    };

    let mut probes = Vec::new();
    for outcome in &evaluation.engineering.outcomes {
        let (outcome_kind, note) = match &outcome.state {
            OutcomeState::Violated { .. } | OutcomeState::CategoricalMismatch { .. } => {
                (ProbeOutcome::FailureFound, outcome_note(&outcome.state))
            }
            OutcomeState::Satisfied { .. } => {
                (ProbeOutcome::NoFailureFound, "limit satisfied".to_string())
            }
            _ => (ProbeOutcome::NotApplicable, outcome_note(&outcome.state)),
        };
        probes.push(CounterexampleProbe {
            probe: format!(
                "{} under the frozen requirement set",
                outcome.requirement_id
            ),
            regime: requirements.scope.clone(),
            outcome: outcome_kind,
            evidence: vec![format!("{}: {note}", outcome.requirement_id)],
        });
    }
    let knowledge_registry = KnowledgeRegistry::bundled()?;
    for authority in authorities {
        let Some(pack) = knowledge_registry.get(&authority.pack_id, &authority.version) else {
            continue;
        };
        for trap in &pack.known_traps {
            let relevant = evaluation.engineering.outcomes.iter().any(|outcome| {
                !outcome.state.is_satisfied() && trap_mentions(trap, &outcome.requirement_id)
            });
            probes.push(CounterexampleProbe {
                probe: trap.trap.clone(),
                regime: trap.why_shallow_fails.clone(),
                outcome: if relevant {
                    ProbeOutcome::FailureFound
                } else {
                    ProbeOutcome::NoFailureFound
                },
                evidence: vec![format!("{}@{}#{}", pack.id, pack.version, trap.id)],
            });
            if probes.len() >= 64 {
                break;
            }
        }
    }

    let mut realization = Vec::new();
    for category in [
        RealizationCategory::Manufacturing,
        RealizationCategory::Reliability,
        RealizationCategory::Sourcing,
        RealizationCategory::Cost,
        RealizationCategory::Test,
    ] {
        let (statement, availability) = match category {
            RealizationCategory::Manufacturing => (
                format!(
                    "processes [{}] assessed for supplier {}",
                    candidate.processes.join(", "),
                    candidate.supplier.0
                ),
                ValueState::Known("candidate process list is revision-bound".to_string()),
            ),
            RealizationCategory::Reliability => (
                format!(
                    "{} unresolved engineering claims derive {} tests",
                    evaluation.engineering.unresolved().len(),
                    evaluation.derived_tests.len()
                ),
                ValueState::Known("tests are listed in the verification gate".to_string()),
            ),
            RealizationCategory::Sourcing => (
                format!(
                    "landed per-unit cost {} and lead time {}",
                    money_text(&evaluation.landed_per_unit),
                    evaluation
                        .lead_time_days
                        .as_known()
                        .map(|days| format!("{days} days"))
                        .unwrap_or_else(|| "unavailable".to_string())
                ),
                ValueState::Known("TCO lines and lead-time basis are recorded".to_string()),
            ),
            RealizationCategory::Cost => (
                format!(
                    "risk-adjusted landed total {}",
                    money_text(&evaluation.landed_total)
                ),
                ValueState::Known("TCO includes MOQ, freight, quality and cash terms".to_string()),
            ),
            RealizationCategory::Test => (
                "derived tests must run before a qualified alternate is released".to_string(),
                ValueState::Known("verification gate lists every derived test".to_string()),
            ),
        };
        realization.push(RealizationConsequence {
            category,
            statement,
            availability,
            evidence: evidence_ids.clone(),
        });
    }

    let mut tests: Vec<FalsifiableTest> = evaluation
        .derived_tests
        .iter()
        .take(64)
        .map(DerivedTest::falsifiable)
        .collect();
    if tests.is_empty() {
        for requirement in requirements
            .requirements
            .iter()
            .filter(|requirement| requirement.mandatory)
            .take(64)
        {
            tests.push(FalsifiableTest {
                claim: requirement.statement.clone(),
                procedure: "measure the characteristic at the stated conditions with a calibrated instrument"
                    .to_string(),
                pass_criterion: "the measured value satisfies the requirement limit".to_string(),
                refutation: "a measurement that violates the requirement limit".to_string(),
                instrument: "calibrated measurement system".to_string(),
                evidence: vec![requirement.id.clone()],
            });
        }
    }

    let selection_digest = selection
        .is_all_selected()
        .then(|| selection.digest())
        .transpose()?;
    let (response_receipt, response_cross_check) = build_receipts(
        request,
        requirements,
        std::slice::from_ref(evaluation),
        authorities,
        evidence,
        selection_digest,
        signer,
        Instant::now(),
    )?;
    let response = EngineeringResponse {
        formulation: Gate::established(formulation),
        authority: Gate::established(knowledge),
        theory,
        validity,
        dimensions: Gate::established(DimensionChecks {
            checks: dimension_checks,
        }),
        executed_analysis: Gate::established(ExecutedAnalysis {
            receipts: vec![response_receipt],
        }),
        cross_check: Gate::established(CrossCheck {
            method: "recompute every limit comparison on SI base values".to_string(),
            independent_of: "faktor-supply-engineering.loop".to_string(),
            receipts: vec![response_cross_check],
            agreement: Agreement {
                metric: "verdict disagreement count".to_string(),
                value: 0.0,
                tolerance: 1e-9,
                agrees: true,
            },
        }),
        uncertainty: Gate::established(UncertaintyStatement {
            method: "deterministic limit comparison; unresolved claims stay unavailable"
                .to_string(),
            contributions: uncertainty_contributions,
            coverage: Some(1.0),
            summary: None,
        }),
        counterexample_search: Gate::established(CounterexampleSearch { probes }),
        realization: Gate::established(RealizationConsequences {
            consequences: realization,
        }),
        verification: Gate::established(FalsifiableVerification { tests }),
    };
    Ok(response)
}

fn trap_mentions(trap: &engineering_knowledge::KnownTrap, requirement: &str) -> bool {
    let requirement = requirement.to_ascii_lowercase();
    let text =
        format!("{} {} {}", trap.trap, trap.why_shallow_fails, trap.guard).to_ascii_lowercase();
    requirement
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 3)
        .any(|token| text.contains(token))
}

fn inventory_summary(request: &LoopRequest) -> LoopResult<InventorySummary> {
    let quantity = request.requirement.quantity.get();
    let supplied = request
        .inventory
        .on_hand
        .get()
        .saturating_add(request.inventory.confirmed.get());
    let net = quantity.saturating_sub(supplied);
    Ok(InventorySummary {
        on_hand: request.inventory.on_hand,
        confirmed: request.inventory.confirmed,
        net_requirement: Units::new(net)?,
        safety_stock: request.inventory.safety_stock,
        evidence: request.evidence.iter().take(4).cloned().collect(),
    })
}

fn fit_assessment(evaluation: &CandidateEvaluation) -> FitAssessment {
    let mut satisfied = Vec::new();
    let mut gaps = Vec::new();
    for outcome in &evaluation.engineering.outcomes {
        if outcome.state.is_satisfied() {
            satisfied.push(outcome.requirement_id.clone());
        } else {
            gaps.push(format!(
                "{}: {}",
                outcome.requirement_id,
                outcome_note(&outcome.state)
            ));
        }
    }
    let conditions = evaluation
        .derived_tests
        .iter()
        .map(|test| format!("{}: {}", test.requirement_id, test.claim))
        .take(16)
        .collect();
    FitAssessment {
        satisfied,
        gaps,
        conditions,
    }
}

fn commercial_terms(
    request: &LoopRequest,
    evaluation: &CandidateEvaluation,
    no_relaxation_applied: bool,
) -> Availability<CommercialTermsSummary> {
    let Some(candidate) = request
        .candidates
        .iter()
        .find(|candidate| candidate.id == evaluation.candidate)
    else {
        return Availability::unavailable(UnavailableReason::MissingEvidence, "candidate vanished");
    };
    let Some(quote) = selected_quote(candidate, !no_relaxation_applied) else {
        return Availability::unavailable(UnavailableReason::NotProvided, "supplier did not quote");
    };
    Availability::known(CommercialTermsSummary {
        incoterm: quote.incoterm,
        payment_days: quote.payment_days,
        moq: Some(quote.moq),
        unit_price: quoted_unit_price(quote, request.requirement.quantity),
        valid_until: quote.valid_until,
        evidence: vec![quote.document.clone()],
    })
}

fn quoted_unit_price(quote: &Quote, quantity: Units) -> Availability<Money> {
    let mut selected = None;
    for price in &quote.prices {
        if price.quantity <= quantity {
            selected = Some(price);
        }
    }
    match selected.or_else(|| quote.prices.first()) {
        Some(price) => price.unit_price.value.clone(),
        None => {
            Availability::unavailable(UnavailableReason::NoCandidate, "quote has no volume price")
        }
    }
}

fn bounded_missing(mut items: Vec<String>) -> Vec<String> {
    items.sort();
    items.dedup();
    items.truncate(MAX_MISSING_ITEMS);
    if items.is_empty() {
        items.push("no decisive evidence".to_string());
    }
    items
}

fn money_text(value: &Availability<Money>) -> String {
    match value {
        Availability::Known { value } => value.to_string(),
        Availability::Unavailable { unavailable } => {
            format!("unavailable({})", unavailable.reason.as_str())
        }
    }
}

fn delta_text(left: &Availability<Money>, right: &Availability<Money>) -> String {
    match (left, right) {
        (Availability::Known { value: left }, Availability::Known { value: right }) => {
            match left.checked_sub(*right) {
                Ok(value) => value.to_string(),
                Err(error) => format!("arithmetic: {error}"),
            }
        }
        _ => "unavailable".to_string(),
    }
}

fn outcome_note(state: &OutcomeState) -> String {
    match state {
        OutcomeState::Satisfied { actual } => format!("satisfied by {actual}"),
        OutcomeState::Violated { actual, limit } => format!("{actual} violates {limit}"),
        OutcomeState::Unavailable { reason } => format!("unavailable: {reason}"),
        OutcomeState::DimensionMismatch { expected, actual } => {
            format!("dimension mismatch: expected {expected}, got {actual}")
        }
        OutcomeState::ModelInvalid { reason } => format!("model invalid: {reason}"),
        OutcomeState::CategoricalMismatch { actual, allowed } => {
            format!("{actual} is not in {allowed:?}")
        }
    }
}

fn push_evidence(evidence: &mut Vec<EvidenceRef>, reference: EvidenceRef) {
    if evidence.len() >= MAX_DECISION_EVIDENCE {
        return;
    }
    if !evidence
        .iter()
        .any(|existing| existing.id == reference.id && existing.revision == reference.revision)
    {
        evidence.push(reference);
    }
}

fn collect_basis_evidence(basis: &Basis, evidence: &mut Vec<EvidenceRef>) {
    match basis {
        Basis::Provenance(provenance) => push_evidence(evidence, provenance.evidence.clone()),
        Basis::Assumption(assumption) => {
            push_evidence(evidence, assumption.provenance.evidence.clone())
        }
    }
}

fn collect_basis_assumption(basis: &Basis, assumptions: &mut Vec<Assumption>) {
    if let Basis::Assumption(assumption) = basis {
        assumptions.push(assumption.clone());
    }
}

fn dedup_assumptions(assumptions: &mut Vec<Assumption>) -> LoopResult<()> {
    let mut seen = BTreeSet::new();
    assumptions.retain(|assumption| seen.insert(assumption.id.clone()));
    if assumptions.len() > MAX_DECISION_ASSUMPTIONS {
        return Err(LoopError::Invalid(format!(
            "assumptions exceed {MAX_DECISION_ASSUMPTIONS} after collection"
        )));
    }
    Ok(())
}

fn push_uncertainty(notes: &mut Vec<UncertaintyNote>, note: UncertaintyNote) {
    if notes.len() >= 64 {
        return;
    }
    if notes.len() == 63 {
        notes.push(UncertaintyNote {
            subject: "uncertainty-bound".to_string(),
            class: UncertaintyClass::Unknown,
            note: "additional uncertainty notes were omitted at the 64-note bound".to_string(),
        });
        return;
    }
    notes.push(note);
}

fn engineering_provenance_from_supply(provenance: &faktor_supply_core::Provenance) -> Provenance {
    let kind = match provenance.source {
        faktor_supply_core::SourceKind::SupplierDocument => ProvenanceKind::ManufacturerData,
        faktor_supply_core::SourceKind::TestReport => ProvenanceKind::Test,
        faktor_supply_core::SourceKind::ErpExtract
        | faktor_supply_core::SourceKind::PurchasingRecord => ProvenanceKind::Measurement,
        faktor_supply_core::SourceKind::PublicTariff => ProvenanceKind::Standard,
        faktor_supply_core::SourceKind::EngineeringRequirement => ProvenanceKind::Specification,
        faktor_supply_core::SourceKind::AuditReport => ProvenanceKind::Test,
        faktor_supply_core::SourceKind::InternalEstimate => ProvenanceKind::Assumption,
        faktor_supply_core::SourceKind::MarketQuote => ProvenanceKind::ManufacturerData,
        faktor_supply_core::SourceKind::Calculator => ProvenanceKind::Calculation,
    };
    let mut out = Provenance::new(kind, provenance.evidence.id.clone()).unwrap_or_else(|_| {
        Provenance::new(ProvenanceKind::EngineeringPractice, "supply evidence")
            .expect("static provenance")
    });
    if let Some(digest) = &provenance.evidence.digest {
        if let Ok(digest) = Digest::from_hex(digest) {
            out = out.with_digest(digest);
        }
    }
    out.with_revision(RevisionId::new(provenance.evidence.revision.get().max(1)))
}

fn engineering_provenance_from_evidence(
    reference: EvidenceRef,
    kind: ProvenanceKind,
) -> Provenance {
    let mut provenance = Provenance::new(kind, reference.id).unwrap_or_else(|_| {
        Provenance::new(ProvenanceKind::EngineeringPractice, "evidence").expect("static provenance")
    });
    if let Some(digest) = &reference.digest {
        if let Ok(digest) = Digest::from_hex(digest) {
            provenance = provenance.with_digest(digest);
        }
    }
    provenance.with_revision(RevisionId::new(reference.revision.get().max(1)))
}

fn engineering_evidence_to_supply(evidence: &RequirementEvidence) -> EvidenceRef {
    EvidenceRef {
        id: evidence.id.clone(),
        revision: Revision(evidence.revision.get()),
        digest: evidence.digest.map(|digest| digest.to_hex()),
    }
}

fn digest_payload(
    decision: &SupplyDecision,
    requirements: &EngineeringRequirementSet,
    evaluations: &[CandidateEvaluation],
    ranked: &[CandidateId],
    knowledge: &[AuthorityRef],
) -> LoopResult<Digest> {
    #[derive(Serialize)]
    struct Payload<'a> {
        decision: &'a SupplyDecision,
        requirements: &'a EngineeringRequirementSet,
        evaluations: &'a [CandidateEvaluation],
        ranked: &'a [CandidateId],
        knowledge: &'a [AuthorityRef],
    }
    Ok(Digest::of_json(&Payload {
        decision,
        requirements,
        evaluations,
        ranked,
        knowledge,
    })?)
}
