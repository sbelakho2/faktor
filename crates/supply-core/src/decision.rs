//! The central [`SupplyDecision`] object.
//!
//! A decision is a revision-bound record: requirement, candidates, technical
//! fit, supplier capability, commercial terms, landed cost, quality and
//! continuity risk, logistics, inventory effect, lifecycle risk, assumptions,
//! uncertainty, evidence, counterfactuals and one recommended action. Every
//! summary may be explicitly unavailable; a decision can never invent a
//! missing field, and conflicts are stored, never averaged.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::basis::{
    validate_text, Assumption, Conflict, Day, EvidenceRef, Revision, Unavailable, UncertaintyNote,
    MAX_ID_BYTES, MAX_TEXT_BYTES,
};
use crate::graph::{Incoterm, PartId, SiteId, SupplierId, SupplierRole, TransportMode};
use crate::money::{Currency, Money, Units};
use crate::ExactError;

/// Maximum candidates in one decision.
pub const MAX_CANDIDATES: usize = 64;
/// Maximum assumptions in one decision.
pub const MAX_DECISION_ASSUMPTIONS: usize = 128;
/// Maximum uncertainty notes in one decision.
pub const MAX_UNCERTAINTY_NOTES: usize = 64;
/// Maximum evidence references in one decision.
pub const MAX_DECISION_EVIDENCE: usize = 256;
/// Maximum counterfactuals in one decision.
pub const MAX_COUNTERFACTUALS: usize = 32;
/// Maximum conflicts in one decision.
pub const MAX_CONFLICTS: usize = 64;
/// Maximum missing items in a request-evidence action.
pub const MAX_MISSING_ITEMS: usize = 64;

/// A stable candidate identifier within a decision.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CandidateId(#[schemars(length(min = 1, max = MAX_ID_BYTES))] pub String);

impl CandidateId {
    /// Validate the identifier.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.0, MAX_ID_BYTES, "candidate id")
    }
}

/// What is being sourced.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyRequirement {
    /// Requirement identifier.
    #[schemars(length(min = 1, max = MAX_ID_BYTES))]
    pub id: String,
    /// The part, when the requirement is bound to a graph part.
    pub part: Option<PartId>,
    /// Bounded description of the need.
    #[schemars(length(min = 1, max = MAX_TEXT_BYTES))]
    pub description: String,
    /// Required quantity.
    pub quantity: Units,
    /// Needed-by day.
    pub needed_by: Day,
    /// Processes the part requires.
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub required_processes: Vec<String>,
    /// Qualifications the supplier must hold.
    #[serde(default)]
    #[schemars(length(max = 32))]
    pub required_qualifications: Vec<String>,
    /// Comparison currency.
    pub currency: Currency,
}

impl SupplyRequirement {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.id, MAX_ID_BYTES, "requirement id")?;
        validate_text(&self.description, MAX_TEXT_BYTES, "requirement description")?;
        if self.required_processes.len() > 32 || self.required_qualifications.len() > 32 {
            return Err(ExactError::OutOfBounds {
                what: "requirement lists",
                detail: "more than 32 entries".to_string(),
            });
        }
        for process in &self.required_processes {
            validate_text(process, MAX_TEXT_BYTES, "required process")?;
        }
        for qualification in &self.required_qualifications {
            validate_text(qualification, MAX_TEXT_BYTES, "required qualification")?;
        }
        Ok(())
    }
}

/// One sourcing candidate. The role is copied from the graph, never inferred.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// Candidate identifier.
    pub id: CandidateId,
    /// The supplier node.
    pub supplier: SupplierId,
    /// The supplier role at decision time.
    pub role: SupplierRole,
    /// The site, when a specific site is proposed.
    pub site: Option<SiteId>,
    /// Evidence bound to the candidate record.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

impl Candidate {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        self.id.validate()?;
        if self.evidence.len() > 32 {
            return Err(ExactError::OutOfBounds {
                what: "candidate evidence",
                detail: "more than 32 entries".to_string(),
            });
        }
        for reference in &self.evidence {
            reference.validate()?;
        }
        Ok(())
    }
}

/// Technical fit against the requirement.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FitAssessment {
    /// Requirement clauses satisfied with evidence.
    pub satisfied: Vec<String>,
    /// Requirement clauses not satisfied.
    pub gaps: Vec<String>,
    /// Conditions attached to the fit.
    pub conditions: Vec<String>,
}

/// A capability summary. Engineering-grade detail lives in
/// `faktor-supply-supplier`; this is the decision-level projection.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitySummary {
    /// Supplier role.
    pub role: SupplierRole,
    /// Processes claimed.
    pub processes: Vec<String>,
    /// Qualifications claimed.
    pub qualifications: Vec<String>,
    /// Bounded evidence references.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// Commercial terms projection.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommercialTermsSummary {
    /// Incoterm.
    pub incoterm: Incoterm,
    /// Payment days.
    pub payment_days: u32,
    /// Minimum order quantity, when contracted.
    pub moq: Option<Units>,
    /// Quoted unit price, when known.
    pub unit_price: crate::basis::Availability<Money>,
    /// Quote validity end day.
    pub valid_until: Option<Day>,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// Landed-cost projection.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandedCostSummary {
    /// Exact per-unit landed cost.
    pub per_unit: crate::basis::Availability<Money>,
    /// Exact total landed cost.
    pub total: crate::basis::Availability<Money>,
    /// Terms that were unavailable (never treated as zero).
    #[serde(default)]
    pub missing_terms: Vec<String>,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// A risk projection.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskSummary {
    /// Ordered qualitative level.
    pub level: crate::graph::RiskLevel,
    /// Drivers of the level.
    pub drivers: Vec<String>,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// Logistics projection.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogisticsSummary {
    /// Selected mode.
    pub mode: TransportMode,
    /// Governing incoterm.
    pub incoterm: Incoterm,
    /// Transit days, when known.
    pub transit_days: crate::basis::Availability<u32>,
    /// Chargeable weight in grams, when known.
    pub chargeable_weight_g: crate::basis::Availability<u64>,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// Inventory effect projection.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventorySummary {
    /// On hand at the decision revision.
    pub on_hand: Units,
    /// Confirmed supply inside the planning horizon.
    pub confirmed: Units,
    /// Net requirement after on-hand and confirmed supply.
    pub net_requirement: Units,
    /// Safety stock target.
    pub safety_stock: Units,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// A counterfactual that was tested, with its typed outcome.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counterfactual {
    /// What was varied.
    pub description: String,
    /// What the analysis found.
    pub outcome: String,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

impl Counterfactual {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(
            &self.description,
            MAX_TEXT_BYTES,
            "counterfactual description",
        )?;
        validate_text(&self.outcome, MAX_TEXT_BYTES, "counterfactual outcome")?;
        if self.evidence.len() > 32 {
            return Err(ExactError::OutOfBounds {
                what: "counterfactual evidence",
                detail: "more than 32 entries".to_string(),
            });
        }
        for reference in &self.evidence {
            reference.validate()?;
        }
        Ok(())
    }
}

/// The single recommended action.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecommendedAction {
    /// Proceed with a named candidate.
    Proceed {
        /// The candidate.
        candidate: CandidateId,
    },
    /// Collect the named missing evidence before deciding.
    RequestEvidence {
        /// Missing items.
        missing: Vec<String>,
    },
    /// Escalate to a named authority (customs ruling, engineering, quality).
    Escalate {
        /// Why escalation is required.
        reason: String,
        /// The authority.
        authority: String,
    },
    /// Refuse to proceed.
    Refuse {
        /// Why.
        reason: String,
    },
}

impl RecommendedAction {
    fn validate(&self) -> Result<(), ExactError> {
        match self {
            Self::Proceed { candidate } => candidate.validate(),
            Self::RequestEvidence { missing } => {
                if missing.is_empty() || missing.len() > MAX_MISSING_ITEMS {
                    return Err(ExactError::OutOfBounds {
                        what: "missing items",
                        detail: format!("{} entries", missing.len()),
                    });
                }
                for item in missing {
                    validate_text(item, MAX_TEXT_BYTES, "missing item")?;
                }
                Ok(())
            }
            Self::Escalate { reason, authority } => {
                validate_text(reason, MAX_TEXT_BYTES, "escalation reason")?;
                validate_text(authority, MAX_ID_BYTES, "escalation authority")
            }
            Self::Refuse { reason } => validate_text(reason, MAX_TEXT_BYTES, "refusal reason"),
        }
    }
}

/// The central supply decision.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyDecision {
    /// Stable decision identifier.
    pub decision_id: String,
    /// Revision of the decision (bumped on every re-evaluation).
    pub revision: Revision,
    /// The requirement.
    pub requirement: SupplyRequirement,
    /// Candidates considered.
    pub candidates: Vec<Candidate>,
    /// Technical fit, when assessed.
    pub technical_fit: crate::basis::Availability<FitAssessment>,
    /// Supplier capability, when assessed.
    pub supplier_capability: crate::basis::Availability<CapabilitySummary>,
    /// Commercial terms, when quoted.
    pub commercial_terms: crate::basis::Availability<CommercialTermsSummary>,
    /// Landed cost, when all terms are known.
    pub landed_cost: crate::basis::Availability<LandedCostSummary>,
    /// Quality risk.
    pub quality_risk: crate::basis::Availability<RiskSummary>,
    /// Continuity risk.
    pub continuity_risk: crate::basis::Availability<RiskSummary>,
    /// Logistics plan.
    pub logistics_plan: crate::basis::Availability<LogisticsSummary>,
    /// Inventory effect.
    pub inventory_effect: crate::basis::Availability<InventorySummary>,
    /// Lifecycle risk.
    pub lifecycle_risk: crate::basis::Availability<RiskSummary>,
    /// Assumptions used.
    #[serde(default)]
    pub assumptions: Vec<Assumption>,
    /// Uncertainty disclosures.
    #[serde(default)]
    pub uncertainty: Vec<UncertaintyNote>,
    /// Evidence bound to the decision.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Counterfactual analyses.
    #[serde(default)]
    pub counterfactuals: Vec<Counterfactual>,
    /// One recommended action.
    pub recommended_action: RecommendedAction,
    /// Surfaced conflicts.
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
}

impl SupplyDecision {
    /// Validate every bound and every cross-reference. A malformed decision
    /// is refused rather than rendered as if it were sound.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.decision_id, MAX_ID_BYTES, "decision id")?;
        self.requirement.validate()?;
        if self.candidates.len() > MAX_CANDIDATES {
            return Err(ExactError::OutOfBounds {
                what: "candidates",
                detail: format!("{} entries", self.candidates.len()),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for candidate in &self.candidates {
            candidate.validate()?;
            if !seen.insert(candidate.id.0.clone()) {
                return Err(ExactError::OutOfBounds {
                    what: "candidate ids",
                    detail: format!("duplicate {}", candidate.id.0),
                });
            }
        }
        validate_availability(&self.technical_fit)?;
        validate_availability(&self.supplier_capability)?;
        validate_availability(&self.commercial_terms)?;
        validate_availability(&self.landed_cost)?;
        validate_availability(&self.quality_risk)?;
        validate_availability(&self.continuity_risk)?;
        validate_availability(&self.logistics_plan)?;
        validate_availability(&self.inventory_effect)?;
        validate_availability(&self.lifecycle_risk)?;
        if self.assumptions.len() > MAX_DECISION_ASSUMPTIONS {
            return Err(ExactError::OutOfBounds {
                what: "assumptions",
                detail: format!("{} entries", self.assumptions.len()),
            });
        }
        for assumption in &self.assumptions {
            assumption.validate()?;
        }
        if self.uncertainty.len() > MAX_UNCERTAINTY_NOTES {
            return Err(ExactError::OutOfBounds {
                what: "uncertainty notes",
                detail: format!("{} entries", self.uncertainty.len()),
            });
        }
        for note in &self.uncertainty {
            note.validate()?;
        }
        if self.evidence.len() > MAX_DECISION_EVIDENCE {
            return Err(ExactError::OutOfBounds {
                what: "decision evidence",
                detail: format!("{} entries", self.evidence.len()),
            });
        }
        for reference in &self.evidence {
            reference.validate()?;
        }
        let any_known_summary = self.technical_fit.is_known()
            || self.supplier_capability.is_known()
            || self.commercial_terms.is_known()
            || self.landed_cost.is_known()
            || self.quality_risk.is_known()
            || self.continuity_risk.is_known()
            || self.logistics_plan.is_known()
            || self.inventory_effect.is_known()
            || self.lifecycle_risk.is_known();
        if any_known_summary && self.evidence.is_empty() {
            return Err(ExactError::OutOfBounds {
                what: "decision evidence",
                detail: "a known summary requires at least one evidence reference".to_string(),
            });
        }
        if matches!(self.recommended_action, RecommendedAction::Proceed { .. })
            && self.evidence.is_empty()
        {
            return Err(ExactError::OutOfBounds {
                what: "decision evidence",
                detail: "proceeding requires evidence".to_string(),
            });
        }
        if self.counterfactuals.len() > MAX_COUNTERFACTUALS {
            return Err(ExactError::OutOfBounds {
                what: "counterfactuals",
                detail: format!("{} entries", self.counterfactuals.len()),
            });
        }
        for counterfactual in &self.counterfactuals {
            counterfactual.validate()?;
        }
        if self.conflicts.len() > MAX_CONFLICTS {
            return Err(ExactError::OutOfBounds {
                what: "conflicts",
                detail: format!("{} entries", self.conflicts.len()),
            });
        }
        for conflict in &self.conflicts {
            conflict.validate()?;
        }
        self.recommended_action.validate()?;
        if let RecommendedAction::Proceed { candidate } = &self.recommended_action {
            if !seen.contains(&candidate.0) {
                return Err(ExactError::OutOfBounds {
                    what: "recommended candidate",
                    detail: format!("{} is not a candidate", candidate.0),
                });
            }
        }
        Ok(())
    }
}

fn validate_availability<T>(value: &crate::basis::Availability<T>) -> Result<(), ExactError> {
    value.validate()
}

/// Helper to build an unavailable decision field with no fabricated value.
pub fn unavailable<T>(
    reason: crate::basis::UnavailableReason,
    detail: impl Into<String>,
) -> crate::basis::Availability<T> {
    crate::basis::Availability::Unavailable {
        unavailable: Unavailable::new(reason, detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::basis::Availability;

    fn requirement() -> SupplyRequirement {
        SupplyRequirement {
            id: "req-1".to_string(),
            part: Some(PartId(1)),
            description: "10k 0402 resistors".to_string(),
            quantity: Units::new(10_000).unwrap(),
            needed_by: Day(20_030),
            required_processes: vec!["smt".to_string()],
            required_qualifications: vec!["ppap".to_string()],
            currency: Currency::USD,
        }
    }

    fn candidate(id: &str) -> Candidate {
        Candidate {
            id: CandidateId(id.to_string()),
            supplier: SupplierId(1),
            role: SupplierRole::Manufacturer,
            site: Some(SiteId(1)),
            evidence: vec![],
        }
    }

    fn empty_decision() -> SupplyDecision {
        SupplyDecision {
            decision_id: "dec-1".to_string(),
            revision: Revision(7),
            requirement: requirement(),
            candidates: vec![candidate("a")],
            technical_fit: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "not assessed",
            ),
            supplier_capability: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "not assessed",
            ),
            commercial_terms: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "not quoted",
            ),
            landed_cost: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "no freight",
            ),
            quality_risk: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "no NCR history",
            ),
            continuity_risk: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "no BCP",
            ),
            logistics_plan: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "no route",
            ),
            inventory_effect: unavailable(
                crate::basis::UnavailableReason::MissingEvidence,
                "no inventory",
            ),
            lifecycle_risk: unavailable(crate::basis::UnavailableReason::MissingEvidence, "no PCN"),
            assumptions: vec![],
            uncertainty: vec![],
            evidence: vec![],
            counterfactuals: vec![],
            recommended_action: RecommendedAction::RequestEvidence {
                missing: vec!["quote".to_string()],
            },
            conflicts: vec![],
        }
    }

    #[test]
    fn a_decision_with_typed_unknowns_is_valid() {
        let decision = empty_decision();
        assert!(decision.validate().is_ok());
        assert!(matches!(
            decision.recommended_action,
            RecommendedAction::RequestEvidence { .. }
        ));
    }

    #[test]
    fn proceeding_with_an_unknown_candidate_is_refused() {
        let mut decision = empty_decision();
        decision.recommended_action = RecommendedAction::Proceed {
            candidate: CandidateId("ghost".to_string()),
        };
        assert!(decision.validate().is_err());
    }

    #[test]
    fn duplicate_candidates_are_refused() {
        let mut decision = empty_decision();
        decision.candidates.push(candidate("a"));
        assert!(decision.validate().is_err());
    }

    #[test]
    fn unbounded_evidence_is_refused() {
        let mut decision = empty_decision();
        decision.evidence = (0..=MAX_DECISION_EVIDENCE)
            .map(|index| EvidenceRef::new(&format!("e{index}"), Revision(1), None).unwrap())
            .collect();
        assert!(decision.validate().is_err());
    }

    #[test]
    fn a_known_summary_without_evidence_is_refused() {
        let mut decision = empty_decision();
        decision.commercial_terms = crate::basis::Availability::known(CommercialTermsSummary {
            incoterm: Incoterm::Fob,
            payment_days: 30,
            moq: None,
            unit_price: crate::basis::Availability::Known {
                value: Money::from_micros(1_000_000, Currency::USD).unwrap(),
            },
            valid_until: None,
            evidence: vec![],
        });
        assert!(decision.validate().is_err());
        decision
            .evidence
            .push(EvidenceRef::new("quote", Revision(1), None).unwrap());
        assert!(decision.validate().is_ok());
    }

    #[test]
    fn unknowns_can_be_constructed_without_basis() {
        let value: Availability<Money> = unavailable(
            crate::basis::UnavailableReason::NotProvided,
            "supplier did not quote",
        );
        assert!(!value.is_known());
        assert!(value.validate().is_ok());
    }
}
