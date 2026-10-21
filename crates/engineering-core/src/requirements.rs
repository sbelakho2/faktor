//! Revision-bound engineering requirement and constraint sets, and the
//! deterministic evaluation of a candidate against them.
//!
//! The evaluator is deliberately narrow: it compares typed, dimensioned
//! values, refuses to turn an unknown into a pass, and derives a falsifiable
//! test for every claim it could not establish. It does not call a model and
//! it does not invent values. Supply integration layers may add knowledge-pack
//! test methods and commercialization facts around this kernel, but the
//! engineering verdict itself is computed here.

use std::collections::{BTreeMap, BTreeSet};

use engineering_units::{Dimension, Interval, Quantity, Unit, UnitRegistry};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::error::{require_text, CoreError, CoreResult};
use crate::gate::DimensionCheck;
use crate::graph::NodeId;
use crate::value::{EngineeringValue, Provenance, RevisionId, ValueState};

/// Maximum requirements in one set.
pub const MAX_REQUIREMENTS: usize = 256;
/// Maximum constraints in one set.
pub const MAX_REQUIREMENT_CONSTRAINTS: usize = 64;
/// Maximum model bindings in one set.
pub const MAX_REQUIREMENT_MODELS: usize = 16;
/// Maximum evidence bindings in one set.
pub const MAX_REQUIREMENT_EVIDENCE: usize = 256;
/// Maximum candidate facts in one evaluation.
pub const MAX_CANDIDATE_FACTS: usize = 256;
/// Maximum categorical facts in one candidate.
pub const MAX_CANDIDATE_CATEGORIES: usize = 128;
/// Maximum derived tests from one evaluation.
pub const MAX_DERIVED_TESTS: usize = 128;

/// Engineering discipline a requirement belongs to.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Discipline {
    /// Electrical behaviour.
    Electrical,
    /// Thermal behaviour.
    Thermal,
    /// Dimensional tolerance and process capability.
    Tolerance,
    /// EMC/EMI behaviour.
    Emc,
    /// Reliability and life.
    Reliability,
    /// Solid mechanics, dynamics or fluids.
    Mechanical,
    /// Manufacturing and inspection.
    Manufacturing,
    /// Anything not covered above.
    Other,
}

impl Discipline {
    /// Stable display label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Electrical => "electrical",
            Self::Thermal => "thermal",
            Self::Tolerance => "tolerance",
            Self::Emc => "emc",
            Self::Reliability => "reliability",
            Self::Mechanical => "mechanical",
            Self::Manufacturing => "manufacturing",
            Self::Other => "other",
        }
    }
}

/// A numeric acceptance limit. All quantities carry their own unit; the
/// evaluator converts the candidate value into the limit's unit and refuses
/// a dimension mismatch.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NumericLimit {
    /// Actual >= limit.
    Min {
        /// Minimum acceptable value.
        limit: Quantity,
    },
    /// Actual <= limit.
    Max {
        /// Maximum acceptable value.
        limit: Quantity,
    },
    /// min <= actual <= max.
    Range {
        /// Lower bound.
        min: Quantity,
        /// Upper bound.
        max: Quantity,
    },
    /// |actual - nominal| <= tolerance. Both quantities must share a dimension.
    EqualWithin {
        /// Nominal value.
        nominal: Quantity,
        /// Absolute tolerance.
        tolerance: Quantity,
    },
}

impl NumericLimit {
    /// Validate dimensions and ordering.
    pub fn validate(&self) -> CoreResult<()> {
        match self {
            Self::Min { limit } | Self::Max { limit } => require_quantity(limit),
            Self::Range { min, max } => {
                require_quantity(min)?;
                require_quantity(max)?;
                if min.dimension() != max.dimension() {
                    return Err(CoreError::InvalidValue {
                        reason: format!(
                            "range bounds have different dimensions: {} vs {}",
                            min.dimension(),
                            max.dimension()
                        ),
                    });
                }
                let upper = max.value_in(min.unit())?;
                if min.value() > upper {
                    return Err(CoreError::InvalidValue {
                        reason: format!(
                            "range lower bound {} exceeds upper bound {}",
                            min.value(),
                            upper
                        ),
                    });
                }
                Ok(())
            }
            Self::EqualWithin { nominal, tolerance } => {
                require_quantity(nominal)?;
                require_quantity(tolerance)?;
                if nominal.dimension() != tolerance.dimension() {
                    return Err(CoreError::InvalidValue {
                        reason: format!(
                            "nominal and tolerance have different dimensions: {} vs {}",
                            nominal.dimension(),
                            tolerance.dimension()
                        ),
                    });
                }
                if tolerance.value() < 0.0 {
                    return Err(CoreError::InvalidValue {
                        reason: "tolerance must be non-negative".to_string(),
                    });
                }
                Ok(())
            }
        }
    }

    /// The unit the candidate value is converted into.
    pub fn unit(&self) -> &Unit {
        match self {
            Self::Min { limit } | Self::Max { limit } => limit.unit(),
            Self::Range { min, .. } => min.unit(),
            Self::EqualWithin { nominal, .. } => nominal.unit(),
        }
    }

    /// The dimension every candidate value must share.
    pub fn dimension(&self) -> Dimension {
        match self {
            Self::Min { limit } | Self::Max { limit } => limit.dimension(),
            Self::Range { min, .. } => min.dimension(),
            Self::EqualWithin { nominal, .. } => nominal.dimension(),
        }
    }

    /// Compare a candidate value dimensionally. A unit error is a refused
    /// comparison, never a coerced pass.
    pub fn accepts(&self, actual: &Quantity) -> CoreResult<bool> {
        match self {
            Self::Min { limit } => Ok(actual.value_in(limit.unit())? >= limit.value()),
            Self::Max { limit } => Ok(actual.value_in(limit.unit())? <= limit.value()),
            Self::Range { min, max } => {
                let actual = actual.value_in(min.unit())?;
                let upper = max.value_in(min.unit())?;
                Ok(actual >= min.value() && actual <= upper)
            }
            Self::EqualWithin { nominal, tolerance } => {
                let actual = actual.value_in(nominal.unit())?;
                let tolerance = tolerance.value_in(nominal.unit())?;
                Ok((actual - nominal.value()).abs() <= tolerance)
            }
        }
    }

    /// A bounded description for outcome records.
    pub fn describe(&self) -> String {
        match self {
            Self::Min { limit } => format!(">= {} {}", limit.value(), limit.unit().symbol()),
            Self::Max { limit } => format!("<= {} {}", limit.value(), limit.unit().symbol()),
            Self::Range { min, max } => format!(
                "{}..={} {}",
                min.value(),
                max.value_in(min.unit()).unwrap_or(max.value()),
                min.unit().symbol()
            ),
            Self::EqualWithin { nominal, tolerance } => format!(
                "{} +/- {} {}",
                nominal.value(),
                tolerance
                    .value_in(nominal.unit())
                    .unwrap_or(tolerance.value()),
                nominal.unit().symbol()
            ),
        }
    }
}

fn require_quantity(quantity: &Quantity) -> CoreResult<()> {
    if quantity.value().is_finite() {
        Ok(())
    } else {
        Err(CoreError::InvalidValue {
            reason: "quantity is not finite".to_string(),
        })
    }
}

/// What one requirement asserts.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequirementTarget {
    /// A numeric property with a dimensioned limit.
    Numeric {
        /// Candidate property key.
        property: String,
        /// Acceptance limit.
        limit: NumericLimit,
        /// Optional governing model this limit is valid under.
        model: Option<String>,
    },
    /// A categorical property with an explicit allowed set.
    Categorical {
        /// Candidate property key.
        property: String,
        /// Allowed values (exact, case-sensitive).
        allowed: Vec<String>,
        /// Optional governing model this rule is valid under.
        model: Option<String>,
    },
}

impl RequirementTarget {
    /// Candidate property key.
    pub fn property(&self) -> &str {
        match self {
            Self::Numeric { property, .. } | Self::Categorical { property, .. } => property,
        }
    }

    /// Governing model, when bound.
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Numeric { model, .. } | Self::Categorical { model, .. } => model.as_deref(),
        }
    }

    fn validate(&self) -> CoreResult<()> {
        match self {
            Self::Numeric {
                property, limit, ..
            } => {
                require_text("requirement.property", property, 256)?;
                limit.validate()
            }
            Self::Categorical {
                property, allowed, ..
            } => {
                require_text("requirement.property", property, 256)?;
                if allowed.is_empty() || allowed.len() > 64 {
                    return Err(CoreError::InvalidValue {
                        reason: format!("categorical allowed set has {} entries", allowed.len()),
                    });
                }
                for value in allowed {
                    require_text("requirement.allowed", value, 512)?;
                }
                Ok(())
            }
        }
    }
}

/// One typed requirement.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringRequirement {
    /// Stable requirement id within the set.
    pub id: String,
    /// Falsifiable statement.
    pub statement: String,
    /// Discipline.
    pub discipline: Discipline,
    /// Mandatory requirements gate acceptance; optional gaps produce
    /// conditions and derived tests.
    pub mandatory: bool,
    /// The asserted target.
    pub target: RequirementTarget,
    /// Conditions under which the requirement applies.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Where the requirement came from.
    pub provenance: Provenance,
}

impl EngineeringRequirement {
    fn validate(&self, revision: RevisionId) -> CoreResult<()> {
        require_text("requirement.id", &self.id, 256)?;
        require_text("requirement.statement", &self.statement, 2_048)?;
        self.target.validate()?;
        self.provenance.validate()?;
        if let Some(bound) = self.provenance.revision {
            if bound.get() == 0 || bound > revision {
                return Err(CoreError::InvalidValue {
                    reason: format!(
                        "requirement {} provenance revision {bound} is outside the set revision {revision}",
                        self.id
                    ),
                });
            }
        }
        for condition in &self.conditions {
            require_text("requirement.condition", condition, 1_024)?;
        }
        Ok(())
    }
}

/// A constraint groups requirements and states why they must hold together.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringConstraint {
    /// Stable constraint id.
    pub id: String,
    /// Falsifiable statement.
    pub statement: String,
    /// Discipline.
    pub discipline: Discipline,
    /// Requirements this constraint binds.
    pub requirement_ids: Vec<String>,
    /// Where the constraint came from.
    pub provenance: Provenance,
}

impl EngineeringConstraint {
    fn validate(&self, requirement_ids: &BTreeSet<String>) -> CoreResult<()> {
        require_text("constraint.id", &self.id, 256)?;
        require_text("constraint.statement", &self.statement, 2_048)?;
        if self.requirement_ids.is_empty() || self.requirement_ids.len() > MAX_REQUIREMENTS {
            return Err(CoreError::InvalidValue {
                reason: "a constraint must bind at least one requirement".to_string(),
            });
        }
        for id in &self.requirement_ids {
            if !requirement_ids.contains(id) {
                return Err(CoreError::InvalidValue {
                    reason: format!("constraint {} references unknown requirement {id}", self.id),
                });
            }
        }
        self.provenance.validate()
    }
}

/// A governing model and the validity envelope it may be applied under.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    /// Model id (matching a knowledge pack model or an explicit authority).
    pub model_id: String,
    /// Model version.
    pub version: String,
    /// Equations the model applies.
    pub equation_ids: Vec<String>,
    /// Inclusive temperature envelope in kelvin, when bounded.
    pub temperature: Option<Interval>,
    /// Conditions that must be established.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Conditions that invalidate the model.
    #[serde(default)]
    pub exclusions: Vec<String>,
    /// Knowledge pack that carries the model, when applicable.
    pub pack_id: Option<String>,
    /// Knowledge pack version.
    pub pack_version: Option<String>,
    /// Source.
    pub provenance: Provenance,
}

impl ModelBinding {
    fn validate(&self) -> CoreResult<()> {
        require_text("model.model_id", &self.model_id, 256)?;
        require_text("model.version", &self.version, 128)?;
        if self.equation_ids.is_empty() || self.equation_ids.len() > 128 {
            return Err(CoreError::InvalidValue {
                reason: "a model must cite at least one equation".to_string(),
            });
        }
        for equation in &self.equation_ids {
            require_text("model.equation_id", equation, 256)?;
        }
        if self.pack_id.is_some() != self.pack_version.is_some() {
            return Err(CoreError::InvalidValue {
                reason: "pack id and version must be provided together".to_string(),
            });
        }
        for condition in self.conditions.iter().chain(self.exclusions.iter()) {
            require_text("model.condition", condition, 1_024)?;
        }
        self.provenance.validate()
    }
}

/// A revision-bound evidence binding shared with the supply decision.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementEvidence {
    /// Stable evidence id.
    pub id: String,
    /// Evidence revision.
    pub revision: RevisionId,
    /// Optional content digest.
    pub digest: Option<Digest>,
    /// Source metadata.
    pub provenance: Provenance,
}

impl RequirementEvidence {
    /// Validate the binding.
    pub fn validate(&self) -> CoreResult<()> {
        require_text("evidence.id", &self.id, 512)?;
        if self.revision.get() == 0 {
            return Err(CoreError::InvalidValue {
                reason: "evidence revision must be non-zero".to_string(),
            });
        }
        self.provenance.validate()
    }
}

/// The typed requirement/constraint set. Every member is bound to one
/// revision; an evaluation against another revision is refused.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineeringRequirementSet {
    /// Stable set id.
    pub id: String,
    /// Revision every member is bound to.
    pub revision: RevisionId,
    /// What the set is for.
    pub objective: String,
    /// What is in and out of scope.
    pub scope: String,
    /// Requirements in deterministic evaluation order.
    #[schemars(length(min = 1, max = MAX_REQUIREMENTS))]
    pub requirements: Vec<EngineeringRequirement>,
    /// Constraints binding requirements together.
    #[serde(default)]
    #[schemars(length(max = MAX_REQUIREMENT_CONSTRAINTS))]
    pub constraints: Vec<EngineeringConstraint>,
    /// Governing models and their validity envelopes.
    #[serde(default)]
    #[schemars(length(max = MAX_REQUIREMENT_MODELS))]
    pub models: Vec<ModelBinding>,
    /// Evidence bound to the set.
    #[serde(default)]
    #[schemars(length(max = MAX_REQUIREMENT_EVIDENCE))]
    pub evidence: Vec<RequirementEvidence>,
}

impl EngineeringRequirementSet {
    /// Validate bounds, ids, dimensions and cross-references.
    pub fn validate(&self) -> CoreResult<()> {
        require_text("requirement_set.id", &self.id, 256)?;
        require_text("requirement_set.objective", &self.objective, 2_048)?;
        require_text("requirement_set.scope", &self.scope, 2_048)?;
        if self.revision.get() == 0 {
            return Err(CoreError::InvalidValue {
                reason: "requirement set revision must be non-zero".to_string(),
            });
        }
        if self.requirements.is_empty() || self.requirements.len() > MAX_REQUIREMENTS {
            return Err(CoreError::InvalidValue {
                reason: format!(
                    "requirement set has {} requirements (1..={MAX_REQUIREMENTS})",
                    self.requirements.len()
                ),
            });
        }
        if self.constraints.len() > MAX_REQUIREMENT_CONSTRAINTS
            || self.models.len() > MAX_REQUIREMENT_MODELS
            || self.evidence.len() > MAX_REQUIREMENT_EVIDENCE
        {
            return Err(CoreError::GraphLimit {
                what: "requirement set members",
                limit: MAX_REQUIREMENT_EVIDENCE,
            });
        }
        let mut requirement_ids = BTreeSet::new();
        for requirement in &self.requirements {
            requirement.validate(self.revision)?;
            if !requirement_ids.insert(requirement.id.clone()) {
                return Err(CoreError::InvalidValue {
                    reason: format!("duplicate requirement id {}", requirement.id),
                });
            }
        }
        let model_ids: BTreeSet<&str> = self
            .models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect();
        if model_ids.len() != self.models.len() {
            return Err(CoreError::InvalidValue {
                reason: "duplicate model binding".to_string(),
            });
        }
        for requirement in &self.requirements {
            if let Some(model) = requirement.target.model() {
                if !model_ids.contains(model) {
                    return Err(CoreError::InvalidValue {
                        reason: format!(
                            "requirement {} binds unknown model {model}",
                            requirement.id
                        ),
                    });
                }
            }
        }
        for model in &self.models {
            model.validate()?;
        }
        for constraint in &self.constraints {
            constraint.validate(&requirement_ids)?;
        }
        for evidence in &self.evidence {
            evidence.validate()?;
        }
        Ok(())
    }

    /// The exact binding a human approval must cover to relax this
    /// requirement: the frozen set digest, its revision, the requirement id
    /// and the proposed limit.
    pub fn relaxation_digest(
        &self,
        requirement_id: &str,
        new_limit: &NumericLimit,
    ) -> CoreResult<Digest> {
        #[derive(Serialize)]
        struct Binding<'a> {
            schema: &'static str,
            set: Digest,
            revision: RevisionId,
            requirement_id: &'a str,
            new_limit: &'a NumericLimit,
        }
        Digest::of_json(&Binding {
            schema: "faktor.engineering.relaxation-binding/v1",
            set: Digest::of_json(self)?,
            revision: self.revision,
            requirement_id,
            new_limit,
        })
    }

    /// The exact binding for an acceptance-criteria/requirement change.
    pub fn change_digest(&self, requirement: &EngineeringRequirement) -> CoreResult<Digest> {
        #[derive(Serialize)]
        struct Binding<'a> {
            schema: &'static str,
            set: Digest,
            revision: RevisionId,
            requirement: &'a EngineeringRequirement,
        }
        Digest::of_json(&Binding {
            schema: "faktor.engineering.requirement-change-binding/v1",
            set: Digest::of_json(self)?,
            revision: self.revision,
            requirement,
        })
    }

    /// The exact binding for waiving/downgrading a proof obligation.
    pub fn waiver_digest(&self, requirement_id: &str) -> CoreResult<Digest> {
        #[derive(Serialize)]
        struct Binding<'a> {
            schema: &'static str,
            set: Digest,
            revision: RevisionId,
            requirement_id: &'a str,
        }
        Digest::of_json(&Binding {
            schema: "faktor.engineering.proof-waiver-binding/v1",
            set: Digest::of_json(self)?,
            revision: self.revision,
            requirement_id,
        })
    }

    /// Replace one requirement's numeric limit and publish a new revision.
    /// The old limit survives in the caller's history; this method never
    /// edits a frozen revision in place. A human approval bound to the
    /// current set revision and [`Self::relaxation_digest`] is mandatory.
    #[allow(clippy::too_many_arguments)]
    pub fn relax(
        &self,
        requirement_id: &str,
        new_limit: NumericLimit,
        rationale: &str,
        provenance: Provenance,
        falsifier: &str,
        evidence: RequirementEvidence,
        approval: &faktor_approval::ApprovalToken,
    ) -> CoreResult<Self> {
        self.validate()?;
        new_limit.validate()?;
        let binding = self.relaxation_digest(requirement_id, &new_limit)?;
        approval.require(&faktor_approval::ApprovalCheck::new(
            faktor_approval::ApprovalKind::EngineeringConstraintRelaxation,
            self.id.clone(),
            self.revision.get().to_string(),
            binding.to_hex(),
        ))?;
        require_text("relaxation.rationale", rationale, 2_048)?;
        require_text("relaxation.falsifier", falsifier, 2_048)?;
        evidence.validate()?;
        let mut next = self.clone();
        next.revision = self.revision.next();
        let requirement = next
            .requirements
            .iter_mut()
            .find(|requirement| requirement.id == requirement_id)
            .ok_or_else(|| CoreError::InvalidValue {
                reason: format!("unknown requirement {requirement_id}"),
            })?;
        match &mut requirement.target {
            RequirementTarget::Numeric { limit, .. } => {
                if limit.dimension() != new_limit.dimension() {
                    return Err(CoreError::InvalidValue {
                        reason: format!(
                            "relaxed limit dimension {} does not match {}",
                            new_limit.dimension(),
                            limit.dimension()
                        ),
                    });
                }
                *limit = new_limit;
            }
            RequirementTarget::Categorical { .. } => {
                return Err(CoreError::InvalidValue {
                    reason: "only numeric limits can be relaxed".to_string(),
                });
            }
        }
        requirement.provenance = provenance;
        requirement.statement = format!(
            "{} (relaxed: {rationale}; falsifier: {falsifier})",
            requirement.statement
        );
        if next.evidence.len() >= MAX_REQUIREMENT_EVIDENCE {
            return Err(CoreError::GraphLimit {
                what: "requirement evidence",
                limit: MAX_REQUIREMENT_EVIDENCE,
            });
        }
        next.evidence.push(evidence);
        next.validate()?;
        Ok(next)
    }

    /// Change a requirement or acceptance criterion. A previously verified
    /// constraint can never be rewritten without a human approval bound to
    /// the current set revision and the exact replacement.
    pub fn change_requirement(
        &self,
        requirement: EngineeringRequirement,
        approval: &faktor_approval::ApprovalToken,
    ) -> CoreResult<Self> {
        self.validate()?;
        requirement.validate(self.revision)?;
        let binding = self.change_digest(&requirement)?;
        let kind = if self
            .requirements
            .iter()
            .any(|existing| existing.id == requirement.id)
        {
            faktor_approval::ApprovalKind::EngineeringVerifiedConstraintChange
        } else {
            faktor_approval::ApprovalKind::EngineeringAcceptanceCriteriaChange
        };
        approval.require(&faktor_approval::ApprovalCheck::new(
            kind,
            self.id.clone(),
            self.revision.get().to_string(),
            binding.to_hex(),
        ))?;
        let mut next = self.clone();
        next.revision = self.revision.next();
        match next
            .requirements
            .iter_mut()
            .find(|existing| existing.id == requirement.id)
        {
            Some(existing) => *existing = requirement,
            None => next.requirements.push(requirement),
        }
        next.validate()?;
        Ok(next)
    }

    /// Waive (downgrade to optional) one requirement's proof obligation.
    /// Mandatory gates can only be relaxed by an explicit human approval
    /// bound to the current revision and the exact requirement id.
    pub fn waive_requirement(
        &self,
        requirement_id: &str,
        rationale: &str,
        approval: &faktor_approval::ApprovalToken,
    ) -> CoreResult<Self> {
        self.validate()?;
        require_text("waiver.rationale", rationale, 2_048)?;
        let binding = self.waiver_digest(requirement_id)?;
        approval.require(&faktor_approval::ApprovalCheck::new(
            faktor_approval::ApprovalKind::EngineeringProofObligationWaiver,
            self.id.clone(),
            self.revision.get().to_string(),
            binding.to_hex(),
        ))?;
        let mut next = self.clone();
        next.revision = self.revision.next();
        let requirement = next
            .requirements
            .iter_mut()
            .find(|requirement| requirement.id == requirement_id)
            .ok_or_else(|| CoreError::InvalidValue {
                reason: format!("unknown requirement {requirement_id}"),
            })?;
        requirement.mandatory = false;
        requirement.statement = format!(
            "{} (proof obligation waived: {rationale})",
            requirement.statement
        );
        next.validate()?;
        Ok(next)
    }
}

/// A categorical candidate fact. Text facts carry their own source/revision;
/// they are never folded into a numeric property.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoricalFact {
    /// Exact value.
    pub value: String,
    /// Source.
    pub provenance: Provenance,
    /// Revision the fact is bound to.
    pub revision: RevisionId,
}

impl CategoricalFact {
    fn validate(&self) -> CoreResult<()> {
        require_text("categorical_fact.value", &self.value, 1_024)?;
        if self.revision.get() == 0 {
            return Err(CoreError::InvalidValue {
                reason: "categorical fact revision must be non-zero".to_string(),
            });
        }
        self.provenance.validate()
    }
}

/// One candidate's conditional engineering facts.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateFacts {
    /// Stable candidate id.
    pub candidate_id: String,
    /// Design-graph component node, when the candidate is in a graph.
    pub node: Option<NodeId>,
    /// Revision the facts are bound to.
    pub revision: RevisionId,
    /// Numeric/dimensional properties.
    #[serde(default)]
    pub values: BTreeMap<String, EngineeringValue>,
    /// Categorical properties (dielectric, finish, standard family).
    #[serde(default)]
    pub categorical: BTreeMap<String, CategoricalFact>,
    /// Conditions established for this candidate (test setup, build, regime).
    #[serde(default)]
    pub conditions: Vec<String>,
    /// Candidate temperature when the model envelope is temperature-bound.
    pub temperature: Option<Quantity>,
    /// Evidence bound to the candidate.
    #[serde(default)]
    pub evidence: Vec<RequirementEvidence>,
}

impl CandidateFacts {
    /// Validate bounds, revisions and fact consistency.
    pub fn validate(&self) -> CoreResult<()> {
        require_text("candidate.id", &self.candidate_id, 256)?;
        if self.revision.get() == 0 {
            return Err(CoreError::InvalidValue {
                reason: "candidate revision must be non-zero".to_string(),
            });
        }
        if self.values.len() > MAX_CANDIDATE_FACTS
            || self.categorical.len() > MAX_CANDIDATE_CATEGORIES
            || self.conditions.len() > MAX_REQUIREMENTS
            || self.evidence.len() > MAX_REQUIREMENT_EVIDENCE
        {
            return Err(CoreError::GraphLimit {
                what: "candidate facts",
                limit: MAX_CANDIDATE_FACTS,
            });
        }
        for (name, value) in &self.values {
            require_text("candidate.property", name, 256)?;
            value.validate()?;
            if value.revision != self.revision {
                return Err(CoreError::RevisionMismatch {
                    expected: self.revision,
                    found: value.revision,
                });
            }
        }
        for (name, fact) in &self.categorical {
            require_text("candidate.category", name, 256)?;
            fact.validate()?;
            if fact.revision != self.revision {
                return Err(CoreError::RevisionMismatch {
                    expected: self.revision,
                    found: fact.revision,
                });
            }
        }
        for condition in &self.conditions {
            require_text("candidate.condition", condition, 1_024)?;
        }
        if let Some(temperature) = &self.temperature {
            if temperature.dimension() != Dimension::TEMPERATURE {
                return Err(CoreError::InvalidValue {
                    reason: format!(
                        "candidate temperature must have the temperature dimension, got {}",
                        temperature.dimension()
                    ),
                });
            }
        }
        for evidence in &self.evidence {
            evidence.validate()?;
        }
        Ok(())
    }

    /// Conditions independently established for the candidate. Fact-level
    /// annotations are *claims* about conditions; they are not evidence that
    /// the condition holds.
    fn established(&self) -> BTreeSet<&str> {
        self.conditions.iter().map(String::as_str).collect()
    }
}

/// How one requirement was resolved.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutcomeState {
    /// The candidate satisfies the requirement.
    Satisfied {
        /// The compared value.
        actual: String,
    },
    /// The candidate contradicts the requirement.
    Violated {
        /// The compared value.
        actual: String,
        /// The required limit.
        limit: String,
    },
    /// The evidence is missing or unusable.
    Unavailable {
        /// Why.
        reason: String,
    },
    /// The value's dimension cannot be compared with the limit's dimension.
    DimensionMismatch {
        /// Limit dimension.
        expected: String,
        /// Value dimension.
        actual: String,
    },
    /// The governing model's validity envelope is not established.
    ModelInvalid {
        /// Why.
        reason: String,
    },
    /// A categorical value is outside the allowed set.
    CategoricalMismatch {
        /// The observed value.
        actual: String,
        /// The allowed values.
        allowed: Vec<String>,
    },
}

impl OutcomeState {
    /// True only for a satisfied requirement.
    pub fn is_satisfied(&self) -> bool {
        matches!(self, Self::Satisfied { .. })
    }
}

/// One evaluated requirement.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementOutcome {
    /// Requirement id.
    pub requirement_id: String,
    /// Discipline copied from the requirement.
    pub discipline: Discipline,
    /// Mandatory at decision time.
    pub mandatory: bool,
    /// Resolution.
    pub state: OutcomeState,
    /// Requirement source.
    pub provenance: Provenance,
    /// Governing model, when any.
    pub model: Option<String>,
}

/// The deterministic engineering verdict.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EngineeringVerdict {
    /// Every mandatory and optional requirement is satisfied.
    Accept,
    /// Mandatory requirements are satisfied; optional gaps or tests remain.
    Qualify,
    /// At least one mandatory requirement is violated.
    Reject,
    /// At least one mandatory requirement cannot be decided with the
    /// available evidence or model validity.
    Unavailable,
}

impl EngineeringVerdict {
    /// True when the candidate may be promoted with the recorded conditions.
    pub const fn promotable(self) -> bool {
        matches!(self, Self::Accept | Self::Qualify)
    }

    /// Stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Qualify => "qualify",
            Self::Reject => "reject",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Why a test is derived.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TestKind {
    /// A measurement.
    Measurement,
    /// A dimensional inspection.
    Inspection,
    /// A supplier qualification audit.
    Qualification,
    /// A calculation or analysis with a receipt.
    Analysis,
    /// A reliability/life test.
    Reliability,
}

/// A falsifiable test derived from an unresolved requirement.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedTest {
    /// Requirement the test resolves.
    pub requirement_id: String,
    /// Test kind.
    pub kind: TestKind,
    /// What is claimed.
    pub claim: String,
    /// Deterministic procedure.
    pub procedure: String,
    /// Pass criterion.
    pub pass_criterion: String,
    /// What observation refutes the claim.
    pub refutation: String,
    /// Instrument or evidence source.
    pub instrument: String,
    /// Knowledge-pack test-method id, when the test came from an authority.
    pub knowledge_ref: Option<String>,
    /// Evidence ids the test must produce.
    pub evidence: Vec<String>,
}

impl DerivedTest {
    /// Convert to the 11-gate falsifiable-test shape.
    pub fn falsifiable(&self) -> crate::gate::FalsifiableTest {
        crate::gate::FalsifiableTest {
            claim: self.claim.clone(),
            procedure: self.procedure.clone(),
            pass_criterion: self.pass_criterion.clone(),
            refutation: self.refutation.clone(),
            instrument: self.instrument.clone(),
            evidence: self.evidence.clone(),
        }
    }
}

/// The complete evaluation for one candidate.
#[derive(JsonSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEvaluation {
    /// Candidate id.
    pub candidate_id: String,
    /// Candidate facts revision.
    pub revision: RevisionId,
    /// Verdict.
    pub verdict: EngineeringVerdict,
    /// Outcomes in requirement order.
    pub outcomes: Vec<RequirementOutcome>,
    /// Tests derived from unresolved requirements.
    pub tests: Vec<DerivedTest>,
    /// Machine-checked dimensional comparisons.
    pub dimension_checks: Vec<DimensionCheck>,
}

impl CandidateEvaluation {
    /// Count outcomes by predicate.
    pub fn count(&self, predicate: impl Fn(&OutcomeState) -> bool) -> usize {
        self.outcomes.iter().filter(|o| predicate(&o.state)).count()
    }

    /// Requirement ids that are not satisfied.
    pub fn unresolved(&self) -> Vec<&RequirementOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| !outcome.state.is_satisfied())
            .collect()
    }
}

/// Evaluate one candidate against a requirement set. Deterministic,
/// bounded, and revision-checked.
pub fn evaluate(
    requirements: &EngineeringRequirementSet,
    candidate: &CandidateFacts,
) -> CoreResult<CandidateEvaluation> {
    requirements.validate()?;
    candidate.validate()?;

    let established = candidate.established();
    let models: BTreeMap<&str, &ModelBinding> = requirements
        .models
        .iter()
        .map(|model| (model.model_id.as_str(), model))
        .collect();

    let mut outcomes = Vec::with_capacity(requirements.requirements.len());
    let mut tests = Vec::new();
    let mut dimension_checks = Vec::new();

    for requirement in &requirements.requirements {
        let model = requirement
            .target
            .model()
            .and_then(|id| models.get(id).copied());
        let model_failure = model.and_then(|model| model_failure(model, candidate, &established));

        let state = if let Some(reason) = model_failure {
            OutcomeState::ModelInvalid { reason }
        } else if let Some(missing) = requirement
            .conditions
            .iter()
            .find(|condition| !established.contains(condition.as_str()))
        {
            OutcomeState::Unavailable {
                reason: format!("condition not established: {missing}"),
            }
        } else if let Some(reason) =
            value_applicability_failure(requirement, candidate, &established)
        {
            OutcomeState::Unavailable { reason }
        } else {
            match &requirement.target {
                RequirementTarget::Numeric {
                    property, limit, ..
                } => match candidate.values.get(property) {
                    None => OutcomeState::Unavailable {
                        reason: format!("property {property} is unavailable"),
                    },
                    Some(value) => evaluate_numeric(property, limit, value, &mut dimension_checks)?,
                },
                RequirementTarget::Categorical {
                    property, allowed, ..
                } => match candidate.categorical.get(property) {
                    None => OutcomeState::Unavailable {
                        reason: format!("category {property} is unavailable"),
                    },
                    Some(fact) if allowed.contains(&fact.value) => OutcomeState::Satisfied {
                        actual: fact.value.clone(),
                    },
                    Some(fact) => OutcomeState::CategoricalMismatch {
                        actual: fact.value.clone(),
                        allowed: allowed.clone(),
                    },
                },
            }
        };

        if !state.is_satisfied() {
            let test = derive_test(requirement, &state);
            if tests.len() < MAX_DERIVED_TESTS {
                tests.push(test);
            }
        }

        outcomes.push(RequirementOutcome {
            requirement_id: requirement.id.clone(),
            discipline: requirement.discipline,
            mandatory: requirement.mandatory,
            state,
            provenance: requirement.provenance.clone(),
            model: requirement.target.model().map(str::to_string),
        });
    }

    let verdict = verdict_for(&outcomes);
    Ok(CandidateEvaluation {
        candidate_id: candidate.candidate_id.clone(),
        revision: candidate.revision,
        verdict,
        outcomes,
        tests,
        dimension_checks,
    })
}

/// A conditional fact is admissible only when every condition it claims is
/// independently established by the candidate. A value's own `condition`
/// annotation never establishes itself: that would let a caller attach an
/// out-of-regime condition to a value and then cite it as satisfied.
fn value_applicability_failure(
    requirement: &EngineeringRequirement,
    candidate: &CandidateFacts,
    established: &BTreeSet<&str>,
) -> Option<String> {
    let RequirementTarget::Numeric { property, .. } = &requirement.target else {
        return None;
    };
    let value = candidate.values.get(property)?;
    if let Some(condition) = &value.condition {
        if !established.contains(condition.as_str()) {
            return Some(format!(
                "property {property} condition not established: {condition}"
            ));
        }
    }
    for condition in &value.applicability.conditions {
        if !established.contains(condition.as_str()) {
            return Some(format!(
                "property {property} applicability condition not established: {condition}"
            ));
        }
    }
    for exclusion in &value.applicability.exclusions {
        if established.contains(exclusion.as_str()) {
            return Some(format!(
                "property {property} does not apply when {exclusion} is established"
            ));
        }
    }
    if let Some(regime) = &value.applicability.load_regime {
        if !established.contains(regime.as_str()) {
            return Some(format!(
                "property {property} load regime not established: {regime}"
            ));
        }
    }
    if value.applicability.temperature.is_some() {
        let temperature = value
            .temperature
            .as_ref()
            .or(candidate.temperature.as_ref());
        let Some(temperature) = temperature else {
            return Some(format!(
                "property {property} applicability is temperature-bound but no temperature is established"
            ));
        };
        match value.applicability.contains_temperature(temperature) {
            Ok(true) => {}
            Ok(false) => {
                return Some(format!(
                    "property {property} temperature is outside its declared applicability"
                ))
            }
            Err(error) => {
                return Some(format!(
                    "property {property} applicability cannot be checked: {error}"
                ))
            }
        }
    }
    None
}

fn model_failure(
    model: &ModelBinding,
    candidate: &CandidateFacts,
    established: &BTreeSet<&str>,
) -> Option<String> {
    if let Some(condition) = model
        .conditions
        .iter()
        .find(|condition| !established.contains(condition.as_str()))
    {
        return Some(format!("model condition not established: {condition}"));
    }
    if let Some(exclusion) = model
        .exclusions
        .iter()
        .find(|exclusion| established.contains(exclusion.as_str()))
    {
        return Some(format!("model exclusion present: {exclusion}"));
    }
    if let Some(bounds) = model.temperature {
        let Some(temperature) = &candidate.temperature else {
            return Some(
                "model is temperature-bound but the candidate temperature is unavailable"
                    .to_string(),
            );
        };
        let units = UnitRegistry::si();
        let kelvin = match units.require("K") {
            Ok(unit) => unit,
            Err(error) => return Some(format!("temperature unit unavailable: {error}")),
        };
        let value = match temperature.unit().convert_to(temperature.value(), &kelvin) {
            Ok(value) => value,
            Err(error) => return Some(format!("temperature conversion failed: {error}")),
        };
        if !bounds.contains(value) {
            return Some(format!(
                "candidate temperature {value} K is outside the model envelope"
            ));
        }
    }
    None
}

fn evaluate_numeric(
    property: &str,
    limit: &NumericLimit,
    value: &EngineeringValue,
    dimension_checks: &mut Vec<DimensionCheck>,
) -> CoreResult<OutcomeState> {
    let quantity = match &value.quantity {
        ValueState::Known(quantity) => quantity,
        ValueState::Unavailable(reason) => {
            return Ok(OutcomeState::Unavailable {
                reason: format!("property {property}: {}", reason.reason),
            });
        }
    };
    if quantity.dimension() != limit.dimension() {
        dimension_checks.push(DimensionCheck::new(
            format!("{property}: candidate vs requirement"),
            limit.dimension(),
            quantity.dimension(),
        )?);
        return Ok(OutcomeState::DimensionMismatch {
            expected: limit.dimension().to_string(),
            actual: quantity.dimension().to_string(),
        });
    }
    dimension_checks.push(DimensionCheck::new(
        format!("{property}: candidate vs requirement"),
        limit.dimension(),
        quantity.dimension(),
    )?);
    match limit.accepts(quantity) {
        Ok(true) => Ok(OutcomeState::Satisfied {
            actual: format!("{} {}", quantity.value(), quantity.unit().symbol()),
        }),
        Ok(false) => Ok(OutcomeState::Violated {
            actual: format!("{} {}", quantity.value(), quantity.unit().symbol()),
            limit: limit.describe(),
        }),
        Err(error) => Ok(OutcomeState::Unavailable {
            reason: format!("property {property} cannot be compared: {error}"),
        }),
    }
}

fn verdict_for(outcomes: &[RequirementOutcome]) -> EngineeringVerdict {
    let mut mandatory_unavailable = false;
    let mut optional_gap = false;
    for outcome in outcomes {
        let mandatory = outcome.mandatory;
        match &outcome.state {
            OutcomeState::Satisfied { .. } => {}
            OutcomeState::Violated { .. } | OutcomeState::CategoricalMismatch { .. } => {
                if mandatory {
                    return EngineeringVerdict::Reject;
                }
                optional_gap = true;
            }
            OutcomeState::DimensionMismatch { .. } => {
                if mandatory {
                    return EngineeringVerdict::Reject;
                }
                optional_gap = true;
            }
            OutcomeState::Unavailable { .. } | OutcomeState::ModelInvalid { .. } => {
                if mandatory {
                    mandatory_unavailable = true;
                } else {
                    optional_gap = true;
                }
            }
        }
    }
    if mandatory_unavailable {
        EngineeringVerdict::Unavailable
    } else if optional_gap {
        EngineeringVerdict::Qualify
    } else {
        EngineeringVerdict::Accept
    }
}

fn derive_test(requirement: &EngineeringRequirement, state: &OutcomeState) -> DerivedTest {
    let (kind, procedure, instrument) = match requirement.discipline {
        Discipline::Tolerance | Discipline::Manufacturing => (
            TestKind::Inspection,
            "measure the characteristic with a calibrated instrument at the stated conditions",
            "calibrated metrology instrument",
        ),
        Discipline::Reliability => (
            TestKind::Reliability,
            "run the load/life test at the stated stress and record failures and drift",
            "reliability test bench",
        ),
        Discipline::Emc => (
            TestKind::Measurement,
            "measure the emission/immunity quantity in the stated setup and bandwidth",
            "EMC receiver and transducer",
        ),
        Discipline::Thermal => (
            TestKind::Measurement,
            "measure the temperature/thermal quantity at the stated load and boundary conditions",
            "calibrated temperature/thermal instrument",
        ),
        Discipline::Electrical => (
            TestKind::Measurement,
            "measure the electrical quantity at the stated bias, frequency and temperature",
            "calibrated electrical instrument",
        ),
        Discipline::Mechanical => (
            TestKind::Measurement,
            "measure the mechanical quantity at the stated load and boundary conditions",
            "calibrated mechanical instrument",
        ),
        Discipline::Other => (
            TestKind::Analysis,
            "recompute the quantity from revision-bound inputs with a recorded receipt",
            "deterministic analysis kernel",
        ),
    };
    let (pass_criterion, refutation) = match state {
        OutcomeState::Violated { limit, .. } => (
            format!("measured value satisfies {limit}"),
            format!("a measurement that violates {limit}"),
        ),
        OutcomeState::CategoricalMismatch { allowed, .. } => (
            format!("observed value is one of {allowed:?}"),
            format!("an observation outside {allowed:?}"),
        ),
        OutcomeState::DimensionMismatch { expected, actual } => (
            format!("value is expressed in a unit with dimension {expected}"),
            format!("a value whose dimension remains {actual}"),
        ),
        OutcomeState::Unavailable { reason } => (
            "the missing fact is supplied with revision-bound evidence".to_string(),
            format!("evidence that contradicts the supplied fact ({reason})"),
        ),
        OutcomeState::ModelInvalid { reason } => (
            "the model's validity envelope is established for this candidate".to_string(),
            format!("evidence that the model does not apply ({reason})"),
        ),
        OutcomeState::Satisfied { .. } => (
            "the requirement remains satisfied".to_string(),
            "a measurement that violates the requirement".to_string(),
        ),
    };
    DerivedTest {
        requirement_id: requirement.id.clone(),
        kind,
        claim: requirement.statement.clone(),
        procedure: procedure.to_string(),
        pass_criterion,
        refutation,
        instrument: instrument.to_string(),
        knowledge_ref: None,
        evidence: vec![requirement.id.clone()],
    }
}

/// Recompute each numeric comparison on SI base values. This is an
/// independent arithmetic path used by the cross-check gate; it returns the
/// same verdict only when the dimensioned comparison and the SI comparison
/// agree.
pub fn si_crosscheck_verdict(
    requirements: &EngineeringRequirementSet,
    candidate: &CandidateFacts,
) -> CoreResult<EngineeringVerdict> {
    requirements.validate()?;
    candidate.validate()?;
    let dimensioned = evaluate(requirements, candidate)?;
    let mut outcomes = Vec::with_capacity(requirements.requirements.len());
    for (requirement, prior) in requirements
        .requirements
        .iter()
        .zip(dimensioned.outcomes.iter())
    {
        let state = match &requirement.target {
            RequirementTarget::Numeric {
                property, limit, ..
            } => match &prior.state {
                // Validity/model/condition failures are not arithmetic
                // disagreements; the independent path must not turn them into
                // a pass.
                OutcomeState::Unavailable { .. }
                | OutcomeState::ModelInvalid { .. }
                | OutcomeState::DimensionMismatch { .. }
                | OutcomeState::CategoricalMismatch { .. } => prior.state.clone(),
                _ => match candidate.values.get(property) {
                    None => OutcomeState::Unavailable {
                        reason: format!("property {property} is unavailable"),
                    },
                    Some(value) => match &value.quantity {
                        ValueState::Unavailable(reason) => OutcomeState::Unavailable {
                            reason: reason.reason.clone(),
                        },
                        ValueState::Known(quantity) => {
                            let actual = quantity.si_value()?;
                            let accepts = match limit {
                                NumericLimit::Min { limit } => actual >= limit.si_value()?,
                                NumericLimit::Max { limit } => actual <= limit.si_value()?,
                                NumericLimit::Range { min, max } => {
                                    actual >= min.si_value()? && actual <= max.si_value()?
                                }
                                NumericLimit::EqualWithin { nominal, tolerance } => {
                                    (actual - nominal.si_value()?).abs() <= tolerance.si_value()?
                                }
                            };
                            if accepts {
                                OutcomeState::Satisfied {
                                    actual: "si-crosscheck".to_string(),
                                }
                            } else {
                                OutcomeState::Violated {
                                    actual: "si-crosscheck".to_string(),
                                    limit: limit.describe(),
                                }
                            }
                        }
                    },
                },
            },
            RequirementTarget::Categorical { .. } => prior.state.clone(),
        };
        outcomes.push(RequirementOutcome {
            requirement_id: requirement.id.clone(),
            discipline: requirement.discipline,
            mandatory: requirement.mandatory,
            state,
            provenance: requirement.provenance.clone(),
            model: requirement.target.model().map(str::to_string),
        });
    }
    Ok(verdict_for(&outcomes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relaxation_approval(
        set: &EngineeringRequirementSet,
        requirement_id: &str,
        limit: &NumericLimit,
    ) -> faktor_approval::ApprovalToken {
        let digest = set.relaxation_digest(requirement_id, limit).unwrap();
        faktor_approval::ApprovalToken::test_token(
            faktor_approval::ApprovalKind::EngineeringConstraintRelaxation,
            set.id.clone(),
            set.revision.get().to_string(),
            digest.to_hex(),
        )
    }
    use crate::value::ProvenanceKind;

    fn revision() -> RevisionId {
        RevisionId::new(7)
    }

    fn provenance() -> Provenance {
        Provenance::new(ProvenanceKind::Specification, "requirement spec rev 7")
            .unwrap()
            .with_revision(revision())
    }

    fn quantity(value: f64, unit: &str) -> Quantity {
        let units = UnitRegistry::si();
        Quantity::new(value, units.require(unit).unwrap()).unwrap()
    }

    fn numeric(
        id: &str,
        property: &str,
        limit: NumericLimit,
        mandatory: bool,
    ) -> EngineeringRequirement {
        EngineeringRequirement {
            id: id.to_string(),
            statement: format!("{property} must satisfy the stated limit"),
            discipline: Discipline::Electrical,
            mandatory,
            target: RequirementTarget::Numeric {
                property: property.to_string(),
                limit,
                model: None,
            },
            conditions: vec![],
            provenance: provenance(),
        }
    }

    fn set(requirements: Vec<EngineeringRequirement>) -> EngineeringRequirementSet {
        EngineeringRequirementSet {
            id: "req-set".to_string(),
            revision: revision(),
            objective: "select a capacitor".to_string(),
            scope: "one alternate at 25 C".to_string(),
            requirements,
            constraints: vec![],
            models: vec![],
            evidence: vec![RequirementEvidence {
                id: "req-doc".to_string(),
                revision: RevisionId::new(1),
                digest: None,
                provenance: provenance(),
            }],
        }
    }

    fn facts(values: &[(&str, f64, &str)]) -> CandidateFacts {
        let mut map = BTreeMap::new();
        for (name, value, unit) in values {
            map.insert(
                name.to_string(),
                EngineeringValue::known(quantity(*value, unit), provenance(), revision()).unwrap(),
            );
        }
        CandidateFacts {
            candidate_id: "candidate".to_string(),
            node: None,
            revision: revision(),
            values: map,
            categorical: BTreeMap::new(),
            conditions: vec![],
            temperature: None,
            evidence: vec![],
        }
    }

    #[test]
    fn a_satisfied_requirement_accepts() {
        let requirements = set(vec![numeric(
            "cap",
            "capacitance",
            NumericLimit::EqualWithin {
                nominal: quantity(10.0, "uF"),
                tolerance: quantity(1.0, "uF"),
            },
            true,
        )]);
        let evaluation = evaluate(&requirements, &facts(&[("capacitance", 10.5, "uF")])).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Accept);
        assert!(evaluation.tests.is_empty());
        assert_eq!(evaluation.dimension_checks.len(), 1);
    }

    #[test]
    fn a_mandatory_violation_rejects_and_derives_a_test() {
        let requirements = set(vec![numeric(
            "esr",
            "esr",
            NumericLimit::Max {
                limit: quantity(100.0, "mohm"),
            },
            true,
        )]);
        let evaluation = evaluate(&requirements, &facts(&[("esr", 250.0, "mohm")])).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Reject);
        assert_eq!(evaluation.tests.len(), 1);
        assert_eq!(evaluation.tests[0].requirement_id, "esr");
    }

    #[test]
    fn a_dimension_mismatch_is_not_a_float_comparison() {
        let requirements = set(vec![numeric(
            "ripple",
            "ripple",
            NumericLimit::Min {
                limit: quantity(1.0, "A"),
            },
            true,
        )]);
        let evaluation = evaluate(&requirements, &facts(&[("ripple", 1.0, "V")])).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Reject);
        assert!(matches!(
            evaluation.outcomes[0].state,
            OutcomeState::DimensionMismatch { .. }
        ));
        assert!(!evaluation.dimension_checks[0].passed());
    }

    #[test]
    fn a_fact_cannot_establish_its_own_conditional_applicability() {
        let mut requirement = numeric(
            "esr",
            "esr",
            NumericLimit::Max {
                limit: quantity(100.0, "mohm"),
            },
            true,
        );
        requirement.conditions = vec!["105 C".to_string()];
        let requirements = set(vec![requirement]);
        let mut candidate = facts(&[("esr", 50.0, "mohm")]);
        candidate.values.get_mut("esr").unwrap().condition = Some("105 C".to_string());
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Unavailable);
        assert!(matches!(
            evaluation.outcomes[0].state,
            OutcomeState::Unavailable { .. }
        ));

        // Control: the same claim is admissible when the candidate actually
        // establishes the condition.
        candidate.conditions = vec!["105 C".to_string()];
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Accept);
    }

    #[test]
    fn value_applicability_exclusions_and_temperature_bounds_are_enforced() {
        let requirements = set(vec![numeric(
            "esr",
            "esr",
            NumericLimit::Max {
                limit: quantity(100.0, "mohm"),
            },
            true,
        )]);
        let mut candidate = facts(&[("esr", 50.0, "mohm")]);
        candidate.conditions = vec!["105 C".to_string()];
        candidate.values.get_mut("esr").unwrap().applicability =
            crate::value::Applicability::unrestricted().with_exclusion("105 C");
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert!(matches!(
            evaluation.outcomes[0].state,
            OutcomeState::Unavailable { .. }
        ));

        let mut candidate = facts(&[("esr", 50.0, "mohm")]);
        candidate.temperature = Some(quantity(500.0, "K"));
        candidate.values.get_mut("esr").unwrap().applicability =
            crate::value::Applicability::unrestricted()
                .with_temperature(engineering_units::Interval::new(273.15, 373.15).unwrap());
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert!(matches!(
            evaluation.outcomes[0].state,
            OutcomeState::Unavailable { .. }
        ));

        candidate.temperature = Some(quantity(300.0, "K"));
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Accept);
    }

    #[test]
    fn a_missing_mandatory_fact_is_unavailable_not_zero() {
        let requirements = set(vec![numeric(
            "esr",
            "esr",
            NumericLimit::Max {
                limit: quantity(100.0, "mohm"),
            },
            true,
        )]);
        let evaluation = evaluate(&requirements, &facts(&[])).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Unavailable);
        assert!(matches!(
            evaluation.outcomes[0].state,
            OutcomeState::Unavailable { .. }
        ));
    }

    #[test]
    fn a_missing_condition_is_unavailable() {
        let mut requirement = numeric(
            "esr",
            "esr",
            NumericLimit::Max {
                limit: quantity(100.0, "mohm"),
            },
            true,
        );
        requirement.conditions = vec!["100 kHz".to_string()];
        let requirements = set(vec![requirement]);
        let evaluation = evaluate(&requirements, &facts(&[("esr", 50.0, "mohm")])).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Unavailable);
    }

    #[test]
    fn a_model_outside_its_envelope_is_unavailable_not_rejected() {
        let mut requirements = set(vec![numeric(
            "esr",
            "esr",
            NumericLimit::Max {
                limit: quantity(100.0, "mohm"),
            },
            true,
        )]);
        if let RequirementTarget::Numeric { model, .. } = &mut requirements.requirements[0].target {
            *model = Some("model-cap".to_string());
        }
        requirements.models.push(ModelBinding {
            model_id: "model-cap".to_string(),
            version: "1.0.0".to_string(),
            equation_ids: vec!["eq-esr".to_string()],
            temperature: Some(Interval::new(233.15, 298.15).unwrap()),
            conditions: vec![],
            exclusions: vec![],
            pack_id: None,
            pack_version: None,
            provenance: provenance(),
        });
        let mut candidate = facts(&[("esr", 50.0, "mohm")]);
        candidate.temperature = Some(quantity(350.0, "K"));
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Unavailable);
        assert!(matches!(
            evaluation.outcomes[0].state,
            OutcomeState::ModelInvalid { .. }
        ));
        assert_eq!(
            si_crosscheck_verdict(&requirements, &candidate).unwrap(),
            EngineeringVerdict::Unavailable
        );
    }

    #[test]
    fn relaxation_publishes_a_new_revision_and_checks_dimensions() {
        let requirements = set(vec![numeric(
            "hole",
            "hole_diameter",
            NumericLimit::EqualWithin {
                nominal: quantity(10.0, "mm"),
                tolerance: quantity(0.010, "mm"),
            },
            true,
        )]);
        let relaxed = requirements
            .relax(
                "hole",
                NumericLimit::EqualWithin {
                    nominal: quantity(10.0, "mm"),
                    tolerance: quantity(0.025, "mm"),
                },
                "process capability study",
                provenance().with_revision(RevisionId::new(8)),
                "a capability study that still shows more than 0.025 mm",
                RequirementEvidence {
                    id: "cpk-study".to_string(),
                    revision: RevisionId::new(2),
                    digest: None,
                    provenance: provenance(),
                },
                &relaxation_approval(
                    &requirements,
                    "hole",
                    &NumericLimit::EqualWithin {
                        nominal: quantity(10.0, "mm"),
                        tolerance: quantity(0.025, "mm"),
                    },
                ),
            )
            .unwrap();
        assert_eq!(relaxed.revision, RevisionId::new(8));
        assert_eq!(relaxed.evidence.len(), 2);
        // A pre-existing requirement keeps its older provenance revision; the
        // set revision is the freeze, not a rewrite of history.
        let mut multi = requirements.clone();
        multi.requirements.push(numeric(
            "voltage",
            "voltage",
            NumericLimit::Min {
                limit: quantity(16.0, "V"),
            },
            true,
        ));
        let multi = multi
            .relax(
                "hole",
                NumericLimit::EqualWithin {
                    nominal: quantity(10.0, "mm"),
                    tolerance: quantity(0.025, "mm"),
                },
                "process capability study",
                provenance().with_revision(RevisionId::new(8)),
                "a capability study that still shows more than 0.025 mm",
                RequirementEvidence {
                    id: "cpk-study-2".to_string(),
                    revision: RevisionId::new(2),
                    digest: None,
                    provenance: provenance(),
                },
                &relaxation_approval(
                    &multi,
                    "hole",
                    &NumericLimit::EqualWithin {
                        nominal: quantity(10.0, "mm"),
                        tolerance: quantity(0.025, "mm"),
                    },
                ),
            )
            .unwrap();
        assert_eq!(multi.requirements.len(), 2);
        let bad = requirements.relax(
            "hole",
            NumericLimit::EqualWithin {
                nominal: quantity(10.0, "mm"),
                tolerance: quantity(0.010, "s"),
            },
            "bad",
            provenance(),
            "bad",
            RequirementEvidence {
                id: "bad".to_string(),
                revision: RevisionId::new(2),
                digest: None,
                provenance: provenance(),
            },
            &relaxation_approval(
                &requirements,
                "hole",
                &NumericLimit::EqualWithin {
                    nominal: quantity(10.0, "mm"),
                    tolerance: quantity(10.0, "s"),
                },
            ),
        );
        assert!(bad.is_err());
    }

    #[test]
    fn relaxation_without_the_bound_human_approval_is_refused() {
        let requirements = set(vec![numeric(
            "hole",
            "hole_diameter",
            NumericLimit::EqualWithin {
                nominal: quantity(10.0, "mm"),
                tolerance: quantity(0.010, "mm"),
            },
            true,
        )]);
        let token = faktor_approval::ApprovalToken::test_token(
            faktor_approval::ApprovalKind::EngineeringConstraintRelaxation,
            requirements.id.clone(),
            requirements.revision.get().to_string(),
            "0".repeat(64),
        );
        let error = requirements
            .relax(
                "hole",
                NumericLimit::EqualWithin {
                    nominal: quantity(10.0, "mm"),
                    tolerance: quantity(0.025, "mm"),
                },
                "process capability study",
                provenance().with_revision(RevisionId::new(8)),
                "a capability study",
                RequirementEvidence {
                    id: "cpk-study".to_string(),
                    revision: RevisionId::new(2),
                    digest: None,
                    provenance: provenance(),
                },
                &token,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            CoreError::Approval(faktor_approval::ApprovalError::Stale { .. })
        ));
    }

    #[test]
    fn si_crosscheck_agrees_with_the_dimensioned_path() {
        let requirements = set(vec![numeric(
            "cap",
            "capacitance",
            NumericLimit::Min {
                limit: quantity(10.0, "uF"),
            },
            true,
        )]);
        let candidate = facts(&[("capacitance", 0.00001, "F")]);
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Accept);
        assert_eq!(
            si_crosscheck_verdict(&requirements, &candidate).unwrap(),
            evaluation.verdict
        );
    }

    #[test]
    fn categorical_values_are_exact_and_never_normalized() {
        let mut requirements = set(vec![]);
        requirements.requirements.push(EngineeringRequirement {
            id: "dielectric".to_string(),
            statement: "dielectric must be X7R".to_string(),
            discipline: Discipline::Electrical,
            mandatory: true,
            target: RequirementTarget::Categorical {
                property: "dielectric".to_string(),
                allowed: vec!["X7R".to_string()],
                model: None,
            },
            conditions: vec![],
            provenance: provenance(),
        });
        let mut candidate = facts(&[]);
        candidate.categorical.insert(
            "dielectric".to_string(),
            CategoricalFact {
                value: "X5R".to_string(),
                provenance: provenance(),
                revision: revision(),
            },
        );
        let evaluation = evaluate(&requirements, &candidate).unwrap();
        assert_eq!(evaluation.verdict, EngineeringVerdict::Reject);
    }
    #[test]
    fn a_relaxation_approval_cannot_be_widened_or_replayed_after_a_set_mutation() {
        let requirements = set(vec![numeric(
            "cap",
            "capacitance",
            NumericLimit::EqualWithin {
                nominal: quantity(10.0, "uF"),
                tolerance: quantity(0.010, "uF"),
            },
            true,
        )]);
        let approved = NumericLimit::EqualWithin {
            nominal: quantity(10.0, "uF"),
            tolerance: quantity(0.025, "uF"),
        };
        let token = relaxation_approval(&requirements, "cap", &approved);
        let wider = NumericLimit::EqualWithin {
            nominal: quantity(10.0, "uF"),
            tolerance: quantity(0.050, "uF"),
        };
        let error = requirements
            .relax(
                "cap",
                wider,
                "wider than approved",
                provenance(),
                "a wider falsifier",
                requirements.evidence[0].clone(),
                &token,
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                CoreError::Approval(faktor_approval::ApprovalError::Stale { .. })
            ),
            "a wider limit must not reuse the narrower approval: {error}"
        );

        let mut mutated = requirements.clone();
        mutated.requirements[0].statement = "a mutated statement".to_string();
        let error = mutated
            .relax(
                "cap",
                approved,
                "post-mutation",
                provenance(),
                "a falsifier",
                mutated.evidence[0].clone(),
                &token,
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                CoreError::Approval(faktor_approval::ApprovalError::Stale { .. })
            ),
            "a set mutation must invalidate the approval: {error}"
        );
    }
}
