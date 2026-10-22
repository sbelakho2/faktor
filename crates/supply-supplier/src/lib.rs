//! faktor-supply-supplier — engineering-grade supplier capability and fit.
//!
//! A supplier is not a name and a country. This crate evaluates manufacturing
//! process coverage, equipment/line capability, capacity windows, lot sizes,
//! tooling, inspection/metrology, process capability (Cpk/Ppk), qualifications,
//! traceability, quality/delivery distributions, authorization and continuity
//! exposure. Every claim is a [`Term`]: known only with evidence or an explicit
//! assumption, otherwise typed unavailable.
//!
//! The fit kernel is deliberately conservative:
//!
//! - a missing mandatory fact is `insufficient_evidence`, never a pass;
//! - contradictory facts are surfaced as a conflict and block a clean fit;
//! - a supplier role is never upgraded (a broker cannot satisfy a
//!   manufacturer requirement, and an "authorized" claim without evidence is
//!   refused at validation time);
//! - Cpk/Ppk and authorization are engineering gates, not marketing scores;
//! - the result is deterministic and bounded.

use std::collections::{BTreeMap, HashMap};

use faktor_supply_core::{
    validate_text, Assumption, AuthorizationStatus, Availability, Basis, Conflict, Day,
    Distribution, EvidenceRef, ExactError, ProcessKind, QualificationKind, QualificationStatus,
    Revision, RiskLevel, SiteId, SupplierId, SupplierRole, Term, Unavailable, UnavailableReason,
    Units, MAX_ID_BYTES, MAX_TEXT_BYTES,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum processes on one profile.
pub const MAX_PROFILE_PROCESSES: usize = 64;
/// Maximum equipment/line records on one profile.
pub const MAX_PROFILE_EQUIPMENT: usize = 128;
/// Maximum capacity windows.
pub const MAX_CAPACITY_WINDOWS: usize = 128;
/// Maximum tooling records.
pub const MAX_TOOLING: usize = 64;
/// Maximum inspection/metrology records.
pub const MAX_INSPECTION: usize = 64;
/// Maximum process-capability records.
pub const MAX_PROCESS_CAPABILITY: usize = 128;
/// Maximum qualification claims.
pub const MAX_QUALIFICATIONS: usize = 32;
/// Maximum continuity exposures.
pub const MAX_CONTINUITY: usize = 32;
/// Maximum capability facts per record.
pub const MAX_FACTS: usize = 128;
/// Maximum site relationships.
pub const MAX_SITE_RELATIONSHIPS: usize = 32;
/// Maximum requirements in one sourcing requirement.
pub const MAX_REQUIREMENTS: usize = 128;

/// Standard capability-fact keys for engineering-grade equipment claims.
/// The evaluator is intentionally key-based: a new machine attribute is a new
/// evidenced fact, not a schema migration.
pub mod capability_keys {
    /// Minimum package (SMT).
    pub const SMT_PACKAGE_MIN: &str = "smt.package_min";
    /// Maximum package (SMT).
    pub const SMT_PACKAGE_MAX: &str = "smt.package_max";
    /// Placement accuracy in micrometres.
    pub const SMT_PLACEMENT_ACCURACY_UM: &str = "smt.placement_accuracy_um";
    /// Feeder capacity.
    pub const SMT_FEEDER_CAPACITY: &str = "smt.feeder_capacity";
    /// Soldering processes supported.
    pub const SMT_SOLDERING: &str = "smt.soldering";
    /// Automated optical inspection.
    pub const SMT_AOI: &str = "smt.aoi";
    /// Solder-paste inspection.
    pub const SMT_SPI: &str = "smt.spi";
    /// X-ray inspection.
    pub const SMT_XRAY: &str = "smt.xray";
    /// In-circuit test.
    pub const SMT_ICT: &str = "smt.ict";
    /// Functional test.
    pub const SMT_FCT: &str = "smt.fct";
    /// Conformal coating.
    pub const SMT_COATING: &str = "smt.coating";
    /// Unit-level traceability during assembly.
    pub const SMT_TRACEABILITY: &str = "smt.traceability";
    /// MSL handling capability.
    pub const SMT_MSL_HANDLING: &str = "smt.msl_handling";
    /// Machining axes.
    pub const MACHINING_AXES: &str = "machining.axes";
    /// Machining X travel in millimetres.
    pub const MACHINING_TRAVEL_X_MM: &str = "machining.travel_x_mm";
    /// Machining Y travel in millimetres.
    pub const MACHINING_TRAVEL_Y_MM: &str = "machining.travel_y_mm";
    /// Machining Z travel in millimetres.
    pub const MACHINING_TRAVEL_Z_MM: &str = "machining.travel_z_mm";
    /// Spindle speed in revolutions per minute.
    pub const MACHINING_SPINDLE_RPM: &str = "machining.spindle_rpm";
    /// Workholding description.
    pub const MACHINING_WORKHOLDING: &str = "machining.workholding";
    /// CMM capability.
    pub const MACHINING_CMM: &str = "machining.cmm";
    /// Achievable surface finish Ra in micrometres x10.
    pub const MACHINING_SURFACE_FINISH_RA_UM_X10: &str = "machining.surface_finish_ra_um_x10";
}

// ------------------------------------------------------------------ facts

/// A typed capability fact value. Units are explicit so comparisons are
/// dimension-checked rather than guessed.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FactValue {
    /// A scalar with its unit (for example `placement_accuracy_um`).
    Number {
        /// Integer value.
        value: i64,
        /// Unit label.
        unit: String,
    },
    /// A bounded text capability (process class, package family).
    Text {
        /// The text.
        value: String,
    },
    /// A boolean capability (AOI present, X-ray present).
    Boolean {
        /// The value.
        value: bool,
    },
    /// An inclusive numeric range with its unit.
    Range {
        /// Lower bound.
        min: i64,
        /// Upper bound.
        max: i64,
        /// Unit label.
        unit: String,
    },
}

impl FactValue {
    /// Validate bounds and range ordering.
    pub fn validate(&self) -> Result<(), ExactError> {
        match self {
            Self::Number { unit, .. } | Self::Range { unit, .. } => {
                validate_text(unit, MAX_ID_BYTES, "fact unit")
            }
            Self::Text { value } => validate_text(value, MAX_TEXT_BYTES, "fact text"),
            Self::Boolean { .. } => Ok(()),
        }
        .and_then(|()| match self {
            Self::Range { min, max, .. } if min > max => Err(ExactError::OutOfBounds {
                what: "fact range",
                detail: format!("{min} > {max}"),
            }),
            _ => Ok(()),
        })
    }

    /// A stable display value for conflict reporting.
    pub fn display(&self) -> String {
        match self {
            Self::Number { value, unit } => format!("{value} {unit}"),
            Self::Text { value } => value.clone(),
            Self::Boolean { value } => value.to_string(),
            Self::Range { min, max, unit } => format!("{min}..={max} {unit}"),
        }
    }

    /// The unit, when numeric.
    pub fn unit(&self) -> Option<&str> {
        match self {
            Self::Number { unit, .. } | Self::Range { unit, .. } => Some(unit),
            _ => None,
        }
    }
}

/// One named capability fact with evidence or an assumption.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityFact {
    /// Stable fact key (lowercase dotted namespace is recommended).
    pub key: String,
    /// The value and its basis.
    pub value: Term<FactValue>,
}

impl CapabilityFact {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.key, MAX_ID_BYTES, "fact key")?;
        self.value.validate()?;
        if let Some(fact) = self.value.as_known() {
            fact.validate()?;
        }
        Ok(())
    }
}

/// How a requirement compares against an evidenced fact.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// Actual >= expected.
    AtLeast,
    /// Actual <= expected.
    AtMost,
    /// Actual equals expected (same unit for numbers).
    Equals,
    /// Actual text contains expected text.
    Contains,
}

/// One capability requirement.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequirement {
    /// Fact key to evaluate.
    pub key: String,
    /// Comparison.
    pub comparison: Comparison,
    /// Expected value.
    pub expected: FactValue,
    /// Mandatory requirements gate the fit; optional ones produce conditions.
    pub mandatory: bool,
}

impl CapabilityRequirement {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.key, MAX_ID_BYTES, "requirement key")?;
        self.expected.validate()
    }
}

/// Process route claim.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCapability {
    /// Process class.
    pub process: ProcessKind,
    /// Bounded description (for example "6-axis milling, 5-axis simultaneous").
    pub description: String,
    /// Structured capability facts.
    #[serde(default)]
    pub facts: Vec<CapabilityFact>,
}

impl ProcessCapability {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.description, MAX_TEXT_BYTES, "process description")?;
        if self.facts.len() > MAX_FACTS {
            return Err(ExactError::OutOfBounds {
                what: "process facts",
                detail: format!("{} entries", self.facts.len()),
            });
        }
        for fact in &self.facts {
            fact.validate()?;
        }
        Ok(())
    }

    /// Find a fact by key.
    pub fn fact(&self, key: &str) -> Option<&CapabilityFact> {
        self.facts.iter().find(|fact| fact.key == key)
    }
}

/// Equipment / line capability record.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EquipmentCapability {
    /// Process served.
    pub process: ProcessKind,
    /// Equipment or line identifier.
    pub equipment: String,
    /// Facts such as package range, placement accuracy, feeder capacity,
    /// soldering type, AOI/SPI/X-ray, ICT/FCT, coating, MSL handling, axes,
    /// travel, spindle, workholding, CMM, surface finish.
    #[serde(default)]
    pub facts: Vec<CapabilityFact>,
}

impl EquipmentCapability {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.equipment, MAX_TEXT_BYTES, "equipment name")?;
        if self.facts.len() > MAX_FACTS {
            return Err(ExactError::OutOfBounds {
                what: "equipment facts",
                detail: format!("{} entries", self.facts.len()),
            });
        }
        for fact in &self.facts {
            fact.validate()?;
        }
        Ok(())
    }
}

/// A capacity booking window. Capacity and commitment are separate terms so
/// an absent commitment is not silently treated as zero.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityWindow {
    /// The site the capacity belongs to, when bound.
    pub site: Option<SiteId>,
    /// The constrained process, when bound.
    pub process: Option<ProcessKind>,
    /// Inclusive start day.
    pub start: Day,
    /// Inclusive end day.
    pub end: Day,
    /// Total capacity in units for the window.
    pub capacity: Term<Units>,
    /// Already committed units in the window.
    pub committed: Term<Units>,
}

impl CapacityWindow {
    /// Validate the window and its terms.
    pub fn validate(&self) -> Result<(), ExactError> {
        if self.end.before(self.start) {
            return Err(ExactError::OutOfBounds {
                what: "capacity window",
                detail: "end before start".to_string(),
            });
        }
        self.capacity.validate()?;
        self.committed.validate()?;
        if let (Some(capacity), Some(committed)) =
            (self.capacity.as_known(), self.committed.as_known())
        {
            if committed.get() > capacity.get() {
                return Err(ExactError::OutOfBounds {
                    what: "capacity commitment",
                    detail: "committed exceeds capacity".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Available units when both terms are known.
    pub fn available(&self) -> Availability<Units> {
        match (self.capacity.value.clone(), self.committed.value.clone()) {
            (Availability::Known { value: capacity }, Availability::Known { value: committed }) => {
                match capacity.checked_sub(committed) {
                    Ok(value) => Availability::Known { value },
                    Err(error) => Availability::Unavailable {
                        unavailable: Unavailable::new(
                            UnavailableReason::Arithmetic,
                            error.to_string(),
                        ),
                    },
                }
            }
            (Availability::Unavailable { unavailable }, _)
            | (_, Availability::Unavailable { unavailable }) => {
                Availability::Unavailable { unavailable }
            }
        }
    }
}

/// Lot-size envelope with an order multiple.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LotSizeRange {
    /// Minimum lot.
    pub min: Units,
    /// Maximum lot.
    pub max: Units,
    /// Order multiple (at least one).
    pub multiple: Units,
}

impl LotSizeRange {
    /// Validate the envelope.
    pub fn validate(&self) -> Result<(), ExactError> {
        if self.min.get() > self.max.get() {
            return Err(ExactError::OutOfBounds {
                what: "lot size",
                detail: "min exceeds max".to_string(),
            });
        }
        if self.multiple.is_zero() {
            return Err(ExactError::OutOfBounds {
                what: "lot multiple",
                detail: "zero".to_string(),
            });
        }
        Ok(())
    }

    /// True when the quantity is representable.
    pub fn contains(&self, quantity: Units) -> bool {
        quantity.get() >= self.min.get()
            && quantity.get() <= self.max.get()
            && quantity.get().is_multiple_of(self.multiple.get())
    }
}

/// Tooling capability.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolingCapability {
    /// Tooling class.
    pub kind: String,
    /// Whether the tooling is in-house.
    pub in_house: Term<bool>,
    /// Tooling lead time.
    pub lead_time_days: Term<u32>,
    /// Tooling cost, when quoted.
    pub cost: Term<faktor_supply_core::Money>,
}

impl ToolingCapability {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.kind, MAX_TEXT_BYTES, "tooling kind")?;
        self.in_house.validate()?;
        self.lead_time_days.validate()?;
        self.cost.validate()
    }
}

/// Inspection / metrology capability.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectionCapability {
    /// Inspection class (CMM, AOI, X-ray, ICT, FCT, SPI, MSA, ...).
    pub kind: String,
    /// Equipment.
    pub equipment: String,
    /// Capability facts.
    #[serde(default)]
    pub facts: Vec<CapabilityFact>,
}

impl InspectionCapability {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.kind, MAX_TEXT_BYTES, "inspection kind")?;
        validate_text(&self.equipment, MAX_TEXT_BYTES, "inspection equipment")?;
        if self.facts.len() > MAX_FACTS {
            return Err(ExactError::OutOfBounds {
                what: "inspection facts",
                detail: format!("{} entries", self.facts.len()),
            });
        }
        for fact in &self.facts {
            fact.validate()?;
        }
        Ok(())
    }
}

/// Process capability index (Cpk or Ppk), scaled by 1,000,000.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCapabilityIndex {
    /// Process.
    pub process: ProcessKind,
    /// Characteristic measured.
    pub characteristic: String,
    /// Cpk scaled by 1e6 (1.33 -> 1_330_000).
    pub cpk: Term<i64>,
    /// Ppk scaled by 1e6.
    pub ppk: Term<i64>,
    /// Sample size.
    pub sample_size: Term<u32>,
    /// Measurement day.
    pub measured_on: Day,
}

impl ProcessCapabilityIndex {
    /// Validate bounds and the sample requirement.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(
            &self.characteristic,
            MAX_TEXT_BYTES,
            "capability characteristic",
        )?;
        self.cpk.validate()?;
        self.ppk.validate()?;
        self.sample_size.validate()?;
        if let Some(sample) = self.sample_size.as_known() {
            if *sample == 0 {
                return Err(ExactError::OutOfBounds {
                    what: "capability sample",
                    detail: "zero samples".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// A qualification claim. Approval is structurally impossible without
/// evidence: validation refuses `Approved` with no evidence reference.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationClaim {
    /// Qualification class.
    pub kind: QualificationKind,
    /// Status.
    pub status: QualificationStatus,
    /// Validity end day.
    pub valid_until: Option<Day>,
    /// Evidence. Required for `Approved`.
    pub evidence: Option<EvidenceRef>,
    /// Revision of the approval record.
    pub revision: Revision,
}

impl QualificationClaim {
    /// Validate the evidence-bound approval law.
    pub fn validate(&self) -> Result<(), ExactError> {
        if let Some(evidence) = &self.evidence {
            evidence.validate()?;
        }
        if self.status == QualificationStatus::Approved && self.evidence.is_none() {
            return Err(ExactError::OutOfBounds {
                what: "qualification",
                detail: "approved without evidence".to_string(),
            });
        }
        if self.status == QualificationStatus::Approved && self.valid_until.is_none() {
            return Err(ExactError::OutOfBounds {
                what: "qualification",
                detail: "approved without a validity date".to_string(),
            });
        }
        Ok(())
    }
}

/// Traceability depth.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TraceabilityLevel {
    /// No traceability.
    None,
    /// Lot / batch traceable.
    Lot,
    /// Date-code traceable.
    DateCode,
    /// Serialized units.
    Serial,
    /// Full as-built genealogy.
    FullAsBuilt,
}

/// Authorization evidence record.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationRecord {
    /// The role being authorized.
    pub role: SupplierRole,
    /// Status.
    pub status: AuthorizationStatus,
    /// Evidence for an authorized (or explicitly not-authorized) claim.
    pub evidence: Option<EvidenceRef>,
    /// Validity end day.
    pub valid_until: Option<Day>,
    /// The authorizing manufacturer, when known.
    pub manufacturer: Option<faktor_supply_core::ManufacturerId>,
}

impl AuthorizationRecord {
    /// Validate the evidence-bound authorization law.
    pub fn validate(&self) -> Result<(), ExactError> {
        if let Some(evidence) = &self.evidence {
            evidence.validate()?;
        }
        if self.status == AuthorizationStatus::Authorized && self.evidence.is_none() {
            return Err(ExactError::OutOfBounds {
                what: "authorization",
                detail: "authorized without evidence".to_string(),
            });
        }
        if self.status == AuthorizationStatus::Authorized && self.valid_until.is_none() {
            return Err(ExactError::OutOfBounds {
                what: "authorization",
                detail: "authorized without validity".to_string(),
            });
        }
        Ok(())
    }

    /// Assess the record at a day. Manufacturer roles do not require a
    /// distributor authorization; every other role's authorization is judged
    /// on evidence and validity, never assumed.
    pub fn assess(&self, as_of: Day) -> AuthorizationAssessment {
        if self.role == SupplierRole::Manufacturer {
            return AuthorizationAssessment {
                state: AuthorizationState::NotRequired,
                detail: "the supplier is the manufacturer".to_string(),
            };
        }
        match self.status {
            AuthorizationStatus::Authorized => match self.valid_until {
                Some(valid_until) if valid_until.before(as_of) => AuthorizationAssessment {
                    state: AuthorizationState::Expired,
                    detail: format!("authorization expired on day {}", valid_until.0),
                },
                Some(_) => AuthorizationAssessment {
                    state: AuthorizationState::Valid,
                    detail: "authorization evidence is current".to_string(),
                },
                None => AuthorizationAssessment {
                    state: AuthorizationState::Unknown,
                    detail: "authorization has no validity date".to_string(),
                },
            },
            AuthorizationStatus::NotAuthorized => AuthorizationAssessment {
                state: AuthorizationState::NotAuthorized,
                detail: "supplier states it is not authorized".to_string(),
            },
            AuthorizationStatus::Unknown => AuthorizationAssessment {
                state: AuthorizationState::Unknown,
                detail: "no authorization evidence".to_string(),
            },
        }
    }
}

/// The assessed authorization state.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationState {
    /// Not required (manufacturer).
    NotRequired,
    /// Valid evidenced authorization.
    Valid,
    /// Expired authorization.
    Expired,
    /// Explicitly not authorized.
    NotAuthorized,
    /// Unknown.
    Unknown,
}

/// The assessment and its bounded explanation.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationAssessment {
    /// State.
    pub state: AuthorizationState,
    /// Explanation.
    pub detail: String,
}

/// Continuity exposure class.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuityKind {
    /// Single manufacturing site.
    SingleSite,
    /// Single tool / line.
    SingleTool,
    /// Natural-disaster exposure.
    NaturalDisasterZone,
    /// Geopolitical / trade exposure.
    Geopolitical,
    /// Financial distress exposure.
    Financial,
    /// Capacity concentration.
    Capacity,
    /// Utility / infrastructure dependency.
    Infrastructure,
}

/// A continuity exposure.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuityExposure {
    /// Exposure class.
    pub kind: ContinuityKind,
    /// Ordered severity.
    pub severity: RiskLevel,
    /// Recovery time after a disruption.
    pub recovery_days: Term<u32>,
    /// Mitigation description.
    pub mitigation: Term<String>,
}

impl ContinuityExposure {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        self.recovery_days.validate()?;
        self.mitigation.validate()?;
        if let Some(mitigation) = self.mitigation.as_known() {
            validate_text(mitigation, MAX_TEXT_BYTES, "continuity mitigation")?;
        }
        Ok(())
    }
}

/// Delivery / quality / responsiveness distributions.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplierMetrics {
    /// On-time delivery, in parts-per-million of deliveries on time.
    pub otd_ppm: Term<Distribution>,
    /// Defect rate, in defective parts per million.
    pub defect_ppm: Term<Distribution>,
    /// NCR rate, in NCRs per million units.
    pub ncr_ppm: Term<Distribution>,
    /// Lead time in days.
    pub lead_time_days: Term<Distribution>,
    /// Response time in hours.
    pub response_hours: Term<Distribution>,
}

impl SupplierMetrics {
    /// Validate every distribution and its domain.
    pub fn validate(&self) -> Result<(), ExactError> {
        for term in [
            &self.otd_ppm,
            &self.defect_ppm,
            &self.ncr_ppm,
            &self.lead_time_days,
            &self.response_hours,
        ] {
            term.validate()?;
            if let Some(distribution) = term.as_known() {
                distribution.validate()?;
            }
        }
        if let Some(otd) = self.otd_ppm.as_known() {
            if otd.p99 > faktor_supply_core::MAX_PPM {
                return Err(ExactError::OutOfBounds {
                    what: "OTD",
                    detail: "OTD quantile exceeds 1,000,000 ppm".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Ownership structure.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    /// Legal owner / group.
    pub owner: String,
    /// Country.
    pub country: String,
    /// Parent, when known.
    pub parent: Option<String>,
}

impl Ownership {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.owner, MAX_TEXT_BYTES, "owner")?;
        validate_text(&self.country, MAX_ID_BYTES, "ownership country")?;
        if let Some(parent) = &self.parent {
            validate_text(parent, MAX_TEXT_BYTES, "parent owner")?;
        }
        Ok(())
    }
}

/// A declared site relationship.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteRelationship {
    /// The site.
    pub site: SiteId,
    /// Relationship (owned, contract-manufactured, warehouse, ...).
    pub relationship: String,
}

impl SiteRelationship {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.relationship, MAX_TEXT_BYTES, "site relationship")
    }
}

/// The complete supplier profile.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplierProfile {
    /// Supplier graph id.
    pub supplier: SupplierId,
    /// Name.
    pub name: String,
    /// Profile revision.
    pub revision: Revision,
    /// Authorization record.
    pub authorization: AuthorizationRecord,
    /// Manufacturing processes claimed.
    #[serde(default)]
    pub processes: Vec<Term<ProcessCapability>>,
    /// Equipment / line records.
    #[serde(default)]
    pub equipment: Vec<Term<EquipmentCapability>>,
    /// Capacity windows.
    #[serde(default)]
    pub capacity: Vec<Term<CapacityWindow>>,
    /// Lot-size envelope.
    pub lot_sizes: Term<LotSizeRange>,
    /// Tooling capability.
    #[serde(default)]
    pub tooling: Vec<Term<ToolingCapability>>,
    /// Inspection / metrology.
    #[serde(default)]
    pub inspection: Vec<Term<InspectionCapability>>,
    /// Process capability submissions.
    #[serde(default)]
    pub process_capability: Vec<ProcessCapabilityIndex>,
    /// Qualification claims.
    #[serde(default)]
    pub qualifications: Vec<QualificationClaim>,
    /// Traceability depth.
    pub traceability: Term<TraceabilityLevel>,
    /// Delivery/quality distributions.
    pub metrics: SupplierMetrics,
    /// Continuity exposures.
    #[serde(default)]
    pub continuity: Vec<Term<ContinuityExposure>>,
    /// Responsiveness distribution (hours).
    pub responsiveness_hours: Term<Distribution>,
    /// Ownership, when known.
    pub ownership: Option<Term<Ownership>>,
    /// Site relationships.
    #[serde(default)]
    pub site_relationships: Vec<Term<SiteRelationship>>,
}

impl SupplierProfile {
    /// Validate every bound and every evidence law.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.name, MAX_TEXT_BYTES, "supplier name")?;
        self.authorization.validate()?;
        validate_term_list(&self.processes, MAX_PROFILE_PROCESSES, "processes")?;
        for term in &self.processes {
            if let Some(process) = term.as_known() {
                process.validate()?;
            }
        }
        validate_term_list(&self.equipment, MAX_PROFILE_EQUIPMENT, "equipment")?;
        for term in &self.equipment {
            if let Some(equipment) = term.as_known() {
                equipment.validate()?;
            }
        }
        validate_term_list(&self.capacity, MAX_CAPACITY_WINDOWS, "capacity")?;
        for term in &self.capacity {
            if let Some(window) = term.as_known() {
                window.validate()?;
            }
        }
        self.lot_sizes.validate()?;
        if let Some(lot_sizes) = self.lot_sizes.as_known() {
            lot_sizes.validate()?;
        }
        validate_term_list(&self.tooling, MAX_TOOLING, "tooling")?;
        for term in &self.tooling {
            if let Some(tooling) = term.as_known() {
                tooling.validate()?;
            }
        }
        validate_term_list(&self.inspection, MAX_INSPECTION, "inspection")?;
        for term in &self.inspection {
            if let Some(inspection) = term.as_known() {
                inspection.validate()?;
            }
        }
        if self.process_capability.len() > MAX_PROCESS_CAPABILITY {
            return Err(ExactError::OutOfBounds {
                what: "process capability records",
                detail: format!("{} entries", self.process_capability.len()),
            });
        }
        for capability in &self.process_capability {
            capability.validate()?;
        }
        if self.qualifications.len() > MAX_QUALIFICATIONS {
            return Err(ExactError::OutOfBounds {
                what: "qualifications",
                detail: format!("{} entries", self.qualifications.len()),
            });
        }
        for qualification in &self.qualifications {
            qualification.validate()?;
        }
        self.traceability.validate()?;
        self.metrics.validate()?;
        validate_term_list(&self.continuity, MAX_CONTINUITY, "continuity")?;
        for term in &self.continuity {
            if let Some(exposure) = term.as_known() {
                exposure.validate()?;
            }
        }
        self.responsiveness_hours.validate()?;
        if let Some(distribution) = self.responsiveness_hours.as_known() {
            distribution.validate()?;
        }
        if let Some(ownership) = &self.ownership {
            ownership.validate()?;
            if let Some(value) = ownership.as_known() {
                value.validate()?;
            }
        }
        validate_term_list(
            &self.site_relationships,
            MAX_SITE_RELATIONSHIPS,
            "site relationships",
        )?;
        for term in &self.site_relationships {
            if let Some(relationship) = term.as_known() {
                relationship.validate()?;
            }
        }
        Ok(())
    }

    /// All known processes, sorted and deduplicated.
    pub fn known_processes(&self) -> Vec<ProcessKind> {
        let mut processes = self
            .processes
            .iter()
            .filter_map(|term| term.as_known().map(|process| process.process))
            .collect::<Vec<_>>();
        processes.sort_by_key(|process| format!("{process:?}"));
        processes.dedup();
        processes
    }
}

fn validate_term_list<T>(
    terms: &[Term<T>],
    max: usize,
    what: &'static str,
) -> Result<(), ExactError> {
    if terms.len() > max {
        return Err(ExactError::OutOfBounds {
            what,
            detail: format!("{} entries", terms.len()),
        });
    }
    for term in terms {
        term.validate()?;
    }
    Ok(())
}

// ----------------------------------------------------------------- requirement

/// A sourcing requirement evaluated against a profile.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcingRequirement {
    /// Roles that may satisfy the requirement. Never empty: the caller must
    /// state whether a broker or marketplace seller is acceptable.
    pub allowed_roles: Vec<SupplierRole>,
    /// Required manufacturing processes.
    #[serde(default)]
    pub required_processes: Vec<ProcessKind>,
    /// Required qualifications.
    #[serde(default)]
    pub required_qualifications: Vec<QualificationKind>,
    /// Capability facts to check.
    #[serde(default)]
    pub facts: Vec<CapabilityRequirement>,
    /// Required quantity.
    pub quantity: Units,
    /// Start of the required capacity window (inclusive).
    pub window_start: Day,
    /// End of the required capacity window (inclusive).
    pub window_end: Day,
    /// Minimum Cpk scaled by 1e6.
    pub minimum_cpk: Option<i64>,
    /// Minimum traceability depth.
    pub minimum_traceability: Option<TraceabilityLevel>,
    /// Maximum defect rate in parts per million.
    pub maximum_defect_ppm: Option<i64>,
    /// Minimum median OTD in parts per million.
    pub minimum_otd_ppm: Option<i64>,
    /// Maximum lead-time P90 in days.
    pub maximum_lead_time_p90_days: Option<i64>,
    /// Maximum acceptable continuity severity.
    pub maximum_continuity_risk: Option<RiskLevel>,
    /// Maximum response time in hours (median).
    pub maximum_response_hours: Option<i64>,
    /// Evaluation day (qualification/authorization validity).
    pub as_of: Day,
}

impl SourcingRequirement {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        if self.allowed_roles.is_empty() {
            return Err(ExactError::OutOfBounds {
                what: "allowed roles",
                detail: "empty; state the acceptable roles".to_string(),
            });
        }
        let total =
            self.required_processes.len() + self.required_qualifications.len() + self.facts.len();
        if total > MAX_REQUIREMENTS {
            return Err(ExactError::OutOfBounds {
                what: "requirements",
                detail: format!("{total} entries"),
            });
        }
        if self.window_end.before(self.window_start) {
            return Err(ExactError::OutOfBounds {
                what: "requirement window",
                detail: "end before start".to_string(),
            });
        }
        for fact in &self.facts {
            fact.validate()?;
        }
        for threshold in [
            self.maximum_defect_ppm,
            self.minimum_otd_ppm,
            self.maximum_lead_time_p90_days,
            self.maximum_response_hours,
        ]
        .into_iter()
        .flatten()
        {
            if threshold < 0 {
                return Err(ExactError::Negative {
                    what: "requirement threshold",
                });
            }
        }
        if let Some(cpk) = self.minimum_cpk {
            if cpk <= 0 {
                return Err(ExactError::OutOfBounds {
                    what: "minimum Cpk",
                    detail: "must be positive".to_string(),
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- assessment

/// The area a check belongs to.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckArea {
    /// Commercial role.
    Role,
    /// Authorization.
    Authorization,
    /// Process coverage.
    Process,
    /// Equipment capability facts.
    Capability,
    /// Capacity.
    Capacity,
    /// Lot size.
    LotSize,
    /// Process capability (Cpk/Ppk).
    ProcessCapability,
    /// Qualification.
    Qualification,
    /// Traceability.
    Traceability,
    /// Delivery performance.
    Delivery,
    /// Quality performance.
    Quality,
    /// Responsiveness.
    Responsiveness,
    /// Continuity.
    Continuity,
    /// Tooling.
    Tooling,
    /// Inspection / metrology.
    Inspection,
}

/// The outcome of one check.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CheckOutcome {
    /// The evidenced requirement is met.
    Satisfied {
        /// The evidence value or statement.
        detail: String,
    },
    /// Evidenced facts contradict the requirement.
    Violated {
        /// Explanation.
        detail: String,
    },
    /// Evidence is missing or unknown.
    Missing {
        /// Typed reason.
        reason: UnavailableReason,
        /// Explanation.
        detail: String,
    },
    /// Several facts conflict; no value is chosen or averaged.
    Conflicting {
        /// The competing values.
        values: Vec<String>,
    },
    /// The check does not apply to this role/case.
    NotApplicable {
        /// Explanation.
        detail: String,
    },
}

/// One evaluated check.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Area.
    pub area: CheckArea,
    /// Subject (fact key, process, qualification, ...).
    pub subject: String,
    /// Whether this check gates the fit.
    pub mandatory: bool,
    /// Outcome.
    pub outcome: CheckOutcome,
}

/// The deterministic verdict.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum FitVerdict {
    /// All mandatory checks satisfied and no optional gap.
    Fit,
    /// All mandatory checks satisfied; optional evidence is missing.
    FitWithConditions,
    /// At least one mandatory check is violated.
    NotFit,
    /// A mandatory check cannot be decided (missing/conflicting evidence).
    InsufficientEvidence,
}

/// The complete fit assessment.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FitReport {
    /// Supplier.
    pub supplier: SupplierId,
    /// Name.
    pub name: String,
    /// Role.
    pub role: SupplierRole,
    /// Profile revision assessed.
    pub revision: Revision,
    /// Verdict.
    pub verdict: FitVerdict,
    /// Authorization assessment.
    pub authorization: AuthorizationAssessment,
    /// Checks in deterministic requirement order.
    pub checks: Vec<Check>,
    /// Evidence referenced by the decisive terms.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Assumptions referenced by the decisive terms.
    #[serde(default)]
    pub assumptions: Vec<Assumption>,
    /// Surfaced conflicts.
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
}

impl FitReport {
    /// Count checks by outcome label.
    pub fn count(&self, label: &str) -> usize {
        self.checks
            .iter()
            .filter(|check| outcome_label(&check.outcome) == label)
            .count()
    }
}

fn outcome_label(outcome: &CheckOutcome) -> &'static str {
    match outcome {
        CheckOutcome::Satisfied { .. } => "satisfied",
        CheckOutcome::Violated { .. } => "violated",
        CheckOutcome::Missing { .. } => "missing",
        CheckOutcome::Conflicting { .. } => "conflicting",
        CheckOutcome::NotApplicable { .. } => "not_applicable",
    }
}

/// Evaluate a profile against a requirement. Deterministic, bounded, and
/// never upgrades missing evidence into a pass.
pub fn assess(
    profile: &SupplierProfile,
    requirement: &SourcingRequirement,
) -> Result<FitReport, ExactError> {
    profile.validate()?;
    requirement.validate()?;

    let mut checks = Vec::new();
    let mut evidence = Vec::new();
    let mut assumptions = Vec::new();
    let mut conflicts = Vec::new();

    for term in &profile.processes {
        collect_basis(term, &mut evidence, &mut assumptions);
        if let Some(process) = term.as_known() {
            for fact in &process.facts {
                collect_basis(&fact.value, &mut evidence, &mut assumptions);
            }
        }
    }
    for term in &profile.equipment {
        collect_basis(term, &mut evidence, &mut assumptions);
        if let Some(equipment) = term.as_known() {
            for fact in &equipment.facts {
                collect_basis(&fact.value, &mut evidence, &mut assumptions);
            }
        }
    }
    for term in &profile.capacity {
        collect_basis(term, &mut evidence, &mut assumptions);
        if let Some(window) = term.as_known() {
            collect_basis(&window.capacity, &mut evidence, &mut assumptions);
            collect_basis(&window.committed, &mut evidence, &mut assumptions);
        }
    }
    for term in &profile.inspection {
        collect_basis(term, &mut evidence, &mut assumptions);
        if let Some(inspection) = term.as_known() {
            for fact in &inspection.facts {
                collect_basis(&fact.value, &mut evidence, &mut assumptions);
            }
        }
    }
    for term in &profile.continuity {
        collect_basis(term, &mut evidence, &mut assumptions);
        if let Some(exposure) = term.as_known() {
            collect_basis(&exposure.recovery_days, &mut evidence, &mut assumptions);
            collect_basis(&exposure.mitigation, &mut evidence, &mut assumptions);
        }
    }
    collect_basis(&profile.lot_sizes, &mut evidence, &mut assumptions);
    collect_basis(&profile.traceability, &mut evidence, &mut assumptions);
    collect_basis(
        &profile.responsiveness_hours,
        &mut evidence,
        &mut assumptions,
    );
    for term in [
        &profile.metrics.otd_ppm,
        &profile.metrics.defect_ppm,
        &profile.metrics.ncr_ppm,
        &profile.metrics.lead_time_days,
        &profile.metrics.response_hours,
    ] {
        collect_basis(term, &mut evidence, &mut assumptions);
    }
    if let Some(authorization) = &profile.authorization.evidence {
        push_evidence(&mut evidence, authorization.clone());
    }

    let authorization = profile.authorization.assess(requirement.as_of);

    checks.push(Check {
        area: CheckArea::Role,
        subject: profile.authorization.role.as_str().to_string(),
        mandatory: true,
        outcome: if requirement
            .allowed_roles
            .contains(&profile.authorization.role)
        {
            CheckOutcome::Satisfied {
                detail: format!("role {} is allowed", profile.authorization.role.as_str()),
            }
        } else {
            CheckOutcome::Violated {
                detail: format!(
                    "role {} is not in the allowed roles",
                    profile.authorization.role.as_str()
                ),
            }
        },
    });

    let authorization_check = match profile.authorization.role {
        SupplierRole::Manufacturer => CheckOutcome::NotApplicable {
            detail: "a manufacturer does not require distributor authorization".to_string(),
        },
        SupplierRole::AuthorizedDistributor => match authorization.state {
            AuthorizationState::Valid => CheckOutcome::Satisfied {
                detail: authorization.detail.clone(),
            },
            AuthorizationState::Expired => CheckOutcome::Missing {
                reason: UnavailableReason::Expired,
                detail: authorization.detail.clone(),
            },
            AuthorizationState::NotAuthorized => CheckOutcome::Violated {
                detail: authorization.detail.clone(),
            },
            AuthorizationState::NotRequired | AuthorizationState::Unknown => {
                CheckOutcome::Missing {
                    reason: UnavailableReason::MissingEvidence,
                    detail: authorization.detail.clone(),
                }
            }
        },
        _ => CheckOutcome::NotApplicable {
            detail: "the allowed role does not claim manufacturer authorization".to_string(),
        },
    };
    checks.push(Check {
        area: CheckArea::Authorization,
        subject: profile.authorization.role.as_str().to_string(),
        mandatory: profile.authorization.role == SupplierRole::AuthorizedDistributor,
        outcome: authorization_check,
    });

    let fact_index = capability_fact_index(profile);
    let known_processes = profile.known_processes();
    for required in &requirement.required_processes {
        let outcome = if known_processes.contains(required) {
            CheckOutcome::Satisfied {
                detail: format!("{required:?} process is evidenced"),
            }
        } else if profile
            .processes
            .iter()
            .any(|term| term.basis().is_none() || !term.is_known())
        {
            CheckOutcome::Missing {
                reason: UnavailableReason::MissingEvidence,
                detail: format!("{required:?} is not evidenced"),
            }
        } else {
            CheckOutcome::Violated {
                detail: format!("{required:?} is not offered"),
            }
        };
        checks.push(Check {
            area: CheckArea::Process,
            subject: format!("{required:?}"),
            mandatory: true,
            outcome,
        });
    }

    for required in &requirement.facts {
        let (outcome, conflict_values) = evaluate_fact_requirement(&fact_index, required);
        if let Some(values) = conflict_values {
            conflicts.push(Conflict {
                subject: format!("capability fact {}", required.key),
                values,
                evidence: evidence.clone(),
                impact: "conflicting capability claims; not averaged".to_string(),
            });
        }
        checks.push(Check {
            area: CheckArea::Capability,
            subject: required.key.clone(),
            mandatory: required.mandatory,
            outcome,
        });
    }

    let capacity_outcome = evaluate_capacity(profile, requirement);
    checks.push(Check {
        area: CheckArea::Capacity,
        subject: "available capacity".to_string(),
        mandatory: true,
        outcome: capacity_outcome,
    });

    let lot_outcome = match profile.lot_sizes.value.clone() {
        Availability::Known { value } => match value.validate() {
            Ok(()) => {
                if value.contains(requirement.quantity) {
                    CheckOutcome::Satisfied {
                        detail: format!(
                            "quantity {} within {}..={} (multiple {})",
                            requirement.quantity.get(),
                            value.min.get(),
                            value.max.get(),
                            value.multiple.get()
                        ),
                    }
                } else {
                    CheckOutcome::Violated {
                        detail: format!(
                            "quantity {} not representable in {}..={} (multiple {})",
                            requirement.quantity.get(),
                            value.min.get(),
                            value.max.get(),
                            value.multiple.get()
                        ),
                    }
                }
            }
            Err(error) => CheckOutcome::Missing {
                reason: UnavailableReason::OutOfRange,
                detail: error.to_string(),
            },
        },
        Availability::Unavailable { unavailable } => CheckOutcome::Missing {
            reason: unavailable.reason,
            detail: unavailable
                .detail
                .unwrap_or_else(|| "lot size unavailable".to_string()),
        },
    };
    checks.push(Check {
        area: CheckArea::LotSize,
        subject: "lot size".to_string(),
        mandatory: true,
        outcome: lot_outcome,
    });

    if let Some(minimum_cpk) = requirement.minimum_cpk {
        let relevant = profile
            .process_capability
            .iter()
            .filter(|capability| {
                requirement.required_processes.is_empty()
                    || requirement.required_processes.contains(&capability.process)
            })
            .collect::<Vec<_>>();
        if relevant.is_empty() {
            checks.push(Check {
                area: CheckArea::ProcessCapability,
                subject: "Cpk/Ppk".to_string(),
                mandatory: true,
                outcome: CheckOutcome::Missing {
                    reason: UnavailableReason::MissingEvidence,
                    detail: "no process-capability submission".to_string(),
                },
            });
        } else {
            let mut values = Vec::new();
            let mut missing = None;
            for capability in &relevant {
                match capability.cpk.value.clone() {
                    Availability::Known { value } => values.push((value, capability)),
                    Availability::Unavailable { unavailable } => {
                        missing.get_or_insert(unavailable);
                    }
                }
            }
            let outcome = if let Some(unavailable) = missing {
                CheckOutcome::Missing {
                    reason: unavailable.reason,
                    detail: unavailable
                        .detail
                        .unwrap_or_else(|| "Cpk unavailable".to_string()),
                }
            } else if values.is_empty() {
                CheckOutcome::Missing {
                    reason: UnavailableReason::MissingEvidence,
                    detail: "Cpk unavailable".to_string(),
                }
            } else if values.iter().any(|(value, _)| *value < minimum_cpk) {
                let worst = values
                    .iter()
                    .map(|(value, _)| *value)
                    .min()
                    .unwrap_or_default();
                CheckOutcome::Violated {
                    detail: format!("Cpk {worst} below required {minimum_cpk} (scaled 1e6)"),
                }
            } else {
                CheckOutcome::Satisfied {
                    detail: format!(
                        "Cpk {} meets required {}",
                        values
                            .iter()
                            .map(|(value, _)| *value)
                            .min()
                            .unwrap_or_default(),
                        minimum_cpk
                    ),
                }
            };
            checks.push(Check {
                area: CheckArea::ProcessCapability,
                subject: "Cpk".to_string(),
                mandatory: true,
                outcome,
            });
        }
    }

    for required in &requirement.required_qualifications {
        let matching = profile
            .qualifications
            .iter()
            .filter(|claim| claim.kind == *required)
            .collect::<Vec<_>>();
        let outcome = evaluate_qualification(required, &matching, requirement.as_of);
        checks.push(Check {
            area: CheckArea::Qualification,
            subject: format!("{required:?}"),
            mandatory: true,
            outcome,
        });
    }

    if let Some(minimum) = requirement.minimum_traceability {
        let outcome = match profile.traceability.value.clone() {
            Availability::Known { value } => {
                if value >= minimum {
                    CheckOutcome::Satisfied {
                        detail: format!("traceability {value:?} meets {minimum:?}"),
                    }
                } else {
                    CheckOutcome::Violated {
                        detail: format!("traceability {value:?} below {minimum:?}"),
                    }
                }
            }
            Availability::Unavailable { unavailable } => CheckOutcome::Missing {
                reason: unavailable.reason,
                detail: unavailable
                    .detail
                    .unwrap_or_else(|| "traceability unavailable".to_string()),
            },
        };
        checks.push(Check {
            area: CheckArea::Traceability,
            subject: "traceability".to_string(),
            mandatory: true,
            outcome,
        });
    }

    if let Some(minimum_otd) = requirement.minimum_otd_ppm {
        checks.push(Check {
            area: CheckArea::Delivery,
            subject: "OTD median".to_string(),
            mandatory: true,
            outcome: evaluate_distribution_threshold(
                &profile.metrics.otd_ppm,
                minimum_otd,
                ThresholdKind::AtLeast,
                "OTD",
            ),
        });
    }

    if let Some(maximum_ppm) = requirement.maximum_defect_ppm {
        checks.push(Check {
            area: CheckArea::Quality,
            subject: "defect PPM median".to_string(),
            mandatory: true,
            outcome: evaluate_distribution_threshold(
                &profile.metrics.defect_ppm,
                maximum_ppm,
                ThresholdKind::AtMost,
                "defect PPM",
            ),
        });
    }

    if let Some(maximum_p90) = requirement.maximum_lead_time_p90_days {
        checks.push(Check {
            area: CheckArea::Delivery,
            subject: "lead-time P90".to_string(),
            mandatory: true,
            outcome: evaluate_distribution_threshold(
                &profile.metrics.lead_time_days,
                maximum_p90,
                ThresholdKind::AtMostP90,
                "lead time P90",
            ),
        });
    }

    if let Some(maximum_response) = requirement.maximum_response_hours {
        checks.push(Check {
            area: CheckArea::Responsiveness,
            subject: "response time median".to_string(),
            mandatory: false,
            outcome: evaluate_distribution_threshold(
                &profile.metrics.response_hours,
                maximum_response,
                ThresholdKind::AtMost,
                "response time",
            ),
        });
    }

    if let Some(maximum_risk) = requirement.maximum_continuity_risk {
        let outcome = evaluate_continuity(profile, maximum_risk, &mut evidence, &mut assumptions);
        checks.push(Check {
            area: CheckArea::Continuity,
            subject: "continuity exposure".to_string(),
            mandatory: true,
            outcome,
        });
    }

    if !profile.tooling.is_empty() {
        let known = profile
            .tooling
            .iter()
            .filter(|term| term.is_known())
            .count();
        checks.push(Check {
            area: CheckArea::Tooling,
            subject: "tooling".to_string(),
            mandatory: false,
            outcome: if known > 0 {
                CheckOutcome::Satisfied {
                    detail: format!("{known} tooling records evidenced"),
                }
            } else {
                CheckOutcome::Missing {
                    reason: UnavailableReason::MissingEvidence,
                    detail: "tooling records are all unknown".to_string(),
                }
            },
        });
    }

    if !profile.inspection.is_empty() {
        let known = profile
            .inspection
            .iter()
            .filter(|term| term.is_known())
            .count();
        checks.push(Check {
            area: CheckArea::Inspection,
            subject: "inspection/metrology".to_string(),
            mandatory: false,
            outcome: if known > 0 {
                CheckOutcome::Satisfied {
                    detail: format!("{known} inspection records evidenced"),
                }
            } else {
                CheckOutcome::Missing {
                    reason: UnavailableReason::MissingEvidence,
                    detail: "inspection records are all unknown".to_string(),
                }
            },
        });
    }

    let verdict = verdict_for(&checks);
    dedup_evidence(&mut evidence);
    dedup_assumptions(&mut assumptions);
    Ok(FitReport {
        supplier: profile.supplier,
        name: profile.name.clone(),
        role: profile.authorization.role,
        revision: profile.revision,
        verdict,
        authorization,
        checks,
        evidence,
        assumptions,
        conflicts,
    })
}

fn verdict_for(checks: &[Check]) -> FitVerdict {
    let mut verdict = FitVerdict::Fit;
    for check in checks {
        match &check.outcome {
            CheckOutcome::Violated { .. } if check.mandatory => return FitVerdict::NotFit,
            CheckOutcome::Conflicting { .. } if check.mandatory => {
                return FitVerdict::InsufficientEvidence;
            }
            CheckOutcome::Missing { .. } if check.mandatory => {
                verdict = FitVerdict::InsufficientEvidence;
            }
            CheckOutcome::Missing { .. } => {
                if verdict == FitVerdict::Fit {
                    verdict = FitVerdict::FitWithConditions;
                }
            }
            CheckOutcome::Conflicting { .. } => {
                if verdict == FitVerdict::Fit {
                    verdict = FitVerdict::FitWithConditions;
                }
            }
            CheckOutcome::Violated { .. }
            | CheckOutcome::Satisfied { .. }
            | CheckOutcome::NotApplicable { .. } => {}
        }
    }
    verdict
}

type CapabilityFactIndex<'a> = HashMap<&'a str, Vec<&'a CapabilityFact>>;

/// Index capability facts once per profile. The insertion order preserves the
/// evaluator's historical scan order (equipment, then processes, then
/// inspection), so conflict values are unchanged while requirement evaluation
/// becomes linear instead of requirements x facts.
fn capability_fact_index(profile: &SupplierProfile) -> CapabilityFactIndex<'_> {
    fn insert<'a>(index: &mut CapabilityFactIndex<'a>, facts: &'a [CapabilityFact]) {
        for fact in facts {
            index.entry(fact.key.as_str()).or_default().push(fact);
        }
    }
    let mut index = CapabilityFactIndex::new();
    for term in &profile.equipment {
        if let Some(equipment) = term.as_known() {
            insert(&mut index, &equipment.facts);
        }
    }
    for term in &profile.processes {
        if let Some(process) = term.as_known() {
            insert(&mut index, &process.facts);
        }
    }
    for term in &profile.inspection {
        if let Some(inspection) = term.as_known() {
            insert(&mut index, &inspection.facts);
        }
    }
    index
}

fn evaluate_fact_requirement(
    index: &CapabilityFactIndex<'_>,
    requirement: &CapabilityRequirement,
) -> (CheckOutcome, Option<Vec<String>>) {
    let facts = index
        .get(requirement.key.as_str())
        .cloned()
        .unwrap_or_default();
    if facts.is_empty() {
        return (
            CheckOutcome::Missing {
                reason: UnavailableReason::MissingEvidence,
                detail: format!("no fact for {}", requirement.key),
            },
            None,
        );
    }
    let mut satisfied = Vec::new();
    let mut violated = Vec::new();
    let mut missing = None;
    let mut values = Vec::new();
    for fact in facts {
        match fact.value.value.clone() {
            Availability::Known { value } => {
                values.push(value.display());
                match compare(&requirement.comparison, &requirement.expected, &value) {
                    Ok(true) => satisfied.push(value.display()),
                    Ok(false) => violated.push(value.display()),
                    Err(unavailable) => {
                        missing.get_or_insert(unavailable);
                    }
                }
            }
            Availability::Unavailable { unavailable } => {
                missing.get_or_insert(unavailable);
            }
        }
    }
    if !satisfied.is_empty() && !violated.is_empty() {
        let mut all = satisfied;
        all.extend(violated);
        all.sort();
        all.dedup();
        return (
            CheckOutcome::Conflicting {
                values: all.clone(),
            },
            Some(all),
        );
    }
    if !violated.is_empty() {
        return (
            CheckOutcome::Violated {
                detail: format!(
                    "evidenced values [{}] do not meet {:?} {}",
                    violated.join(", "),
                    requirement.comparison,
                    requirement.expected.display()
                ),
            },
            None,
        );
    }
    if !satisfied.is_empty() {
        return (
            CheckOutcome::Satisfied {
                detail: format!(
                    "evidenced value [{}] meets {:?} {}",
                    satisfied.join(", "),
                    requirement.comparison,
                    requirement.expected.display()
                ),
            },
            None,
        );
    }
    let unavailable = missing.unwrap_or(Unavailable::bare(UnavailableReason::MissingEvidence));
    (
        CheckOutcome::Missing {
            reason: unavailable.reason,
            detail: unavailable
                .detail
                .unwrap_or_else(|| "fact unavailable".to_string()),
        },
        None,
    )
}

fn compare(
    comparison: &Comparison,
    expected: &FactValue,
    actual: &FactValue,
) -> Result<bool, Unavailable> {
    let unit_mismatch = || {
        Unavailable::new(
            UnavailableReason::Ambiguous,
            format!(
                "unit mismatch: expected {} got {}",
                expected.unit().unwrap_or("none"),
                actual.unit().unwrap_or("none")
            ),
        )
    };
    match (comparison, expected, actual) {
        (
            Comparison::AtLeast | Comparison::AtMost,
            FactValue::Number {
                value: expected,
                unit: expected_unit,
            },
            FactValue::Number {
                value: actual,
                unit: actual_unit,
            },
        ) => {
            if expected_unit != actual_unit {
                return Err(unit_mismatch());
            }
            Ok(match comparison {
                Comparison::AtLeast => actual >= expected,
                Comparison::AtMost => actual <= expected,
                _ => unreachable!(),
            })
        }
        (
            Comparison::Equals,
            FactValue::Number {
                value: expected,
                unit: expected_unit,
            },
            FactValue::Number {
                value: actual,
                unit: actual_unit,
            },
        ) => {
            if expected_unit != actual_unit {
                return Err(unit_mismatch());
            }
            Ok(actual == expected)
        }
        (
            Comparison::Equals,
            FactValue::Text { value: expected },
            FactValue::Text { value: actual },
        ) => Ok(actual == expected),
        (
            Comparison::Contains,
            FactValue::Text { value: expected },
            FactValue::Text { value: actual },
        ) => Ok(actual.contains(expected)),
        (
            Comparison::Equals,
            FactValue::Boolean { value: expected },
            FactValue::Boolean { value: actual },
        ) => Ok(actual == expected),
        (Comparison::Equals, FactValue::Range { .. }, FactValue::Range { .. }) => {
            Ok(expected == actual)
        }
        _ => Err(Unavailable::new(
            UnavailableReason::Ambiguous,
            "comparison types are not dimensionally comparable".to_string(),
        )),
    }
}

fn evaluate_capacity(profile: &SupplierProfile, requirement: &SourcingRequirement) -> CheckOutcome {
    let mut covering: BTreeMap<(Option<SiteId>, Option<String>), Units> = BTreeMap::new();
    for term in &profile.capacity {
        match term.value.clone() {
            Availability::Known { value: window } => {
                let process_matches = window.process.is_none_or(|process| {
                    requirement.required_processes.is_empty()
                        || requirement.required_processes.contains(&process)
                });
                if !process_matches
                    || window.start > requirement.window_start
                    || requirement.window_end > window.end
                {
                    continue;
                }
                let available = match window.available() {
                    Availability::Known { value } => value,
                    Availability::Unavailable { unavailable } => {
                        return CheckOutcome::Missing {
                            reason: unavailable.reason,
                            detail: unavailable
                                .detail
                                .unwrap_or_else(|| "capacity unavailable".to_string()),
                        }
                    }
                };
                let key = (
                    window.site,
                    window.process.map(|process| format!("{process:?}")),
                );
                if covering.insert(key, available).is_some() {
                    return CheckOutcome::Conflicting {
                        values: covering
                            .values()
                            .map(|available| format!("available {}", available.get()))
                            .collect(),
                    };
                }
            }
            Availability::Unavailable { unavailable } => {
                return CheckOutcome::Missing {
                    reason: unavailable.reason,
                    detail: unavailable
                        .detail
                        .unwrap_or_else(|| "capacity unavailable".to_string()),
                };
            }
        }
    }
    if covering.is_empty() {
        return CheckOutcome::Missing {
            reason: UnavailableReason::MissingEvidence,
            detail: "no capacity window covers the required period".to_string(),
        };
    }
    let mut total = Units::ZERO;
    for available in covering.values() {
        match total.checked_add(*available) {
            Ok(value) => total = value,
            Err(error) => {
                return CheckOutcome::Missing {
                    reason: UnavailableReason::Arithmetic,
                    detail: error.to_string(),
                }
            }
        }
    }
    if total.get() >= requirement.quantity.get() {
        CheckOutcome::Satisfied {
            detail: format!(
                "{} units available for {} required",
                total.get(),
                requirement.quantity.get()
            ),
        }
    } else {
        CheckOutcome::Violated {
            detail: format!(
                "{} units available, {} required",
                total.get(),
                requirement.quantity.get()
            ),
        }
    }
}

fn evaluate_qualification(
    required: &QualificationKind,
    claims: &[&QualificationClaim],
    as_of: Day,
) -> CheckOutcome {
    if claims.is_empty() {
        return CheckOutcome::Missing {
            reason: UnavailableReason::MissingEvidence,
            detail: format!("{required:?} not submitted"),
        };
    }
    let mut approved = Vec::new();
    let mut revoked = Vec::new();
    let mut expired = Vec::new();
    let mut pending = Vec::new();
    for claim in claims {
        match claim.status {
            QualificationStatus::Approved => match claim.valid_until {
                Some(valid_until) if valid_until.before(as_of) => expired.push(valid_until),
                Some(_) => approved.push(claim),
                None => pending.push("approved without validity"),
            },
            QualificationStatus::Revoked => revoked.push("revoked"),
            QualificationStatus::Suspended => revoked.push("suspended"),
            QualificationStatus::Expired => expired.push(Day(0)),
            QualificationStatus::NotStarted | QualificationStatus::EvidencePending => {
                pending.push("evidence pending")
            }
        }
    }
    if !approved.is_empty() && !revoked.is_empty() {
        return CheckOutcome::Conflicting {
            values: vec!["approved and current".to_string(), revoked.join(", ")],
        };
    }
    if !approved.is_empty() {
        return CheckOutcome::Satisfied {
            detail: format!("{required:?} approved and current"),
        };
    }
    if !revoked.is_empty() {
        return CheckOutcome::Violated {
            detail: format!("{required:?}: {}", revoked.join(", ")),
        };
    }
    if !expired.is_empty() {
        return CheckOutcome::Missing {
            reason: UnavailableReason::Expired,
            detail: format!("{required:?} approval expired"),
        };
    }
    CheckOutcome::Missing {
        reason: UnavailableReason::MissingEvidence,
        detail: format!("{required:?}: {}", pending.join(", ")),
    }
}

#[derive(Clone, Copy)]
enum ThresholdKind {
    AtLeast,
    AtMost,
    AtMostP90,
}

fn evaluate_distribution_threshold(
    distribution: &Term<Distribution>,
    threshold: i64,
    kind: ThresholdKind,
    label: &str,
) -> CheckOutcome {
    match distribution.value.clone() {
        Availability::Known { value } => {
            if let Err(error) = value.validate() {
                return CheckOutcome::Missing {
                    reason: UnavailableReason::OutOfRange,
                    detail: error.to_string(),
                };
            }
            let observed = match kind {
                ThresholdKind::AtLeast | ThresholdKind::AtMost => value.median(),
                ThresholdKind::AtMostP90 => value.p90,
            };
            let satisfied = match kind {
                ThresholdKind::AtLeast => observed >= threshold,
                ThresholdKind::AtMost | ThresholdKind::AtMostP90 => observed <= threshold,
            };
            if satisfied {
                CheckOutcome::Satisfied {
                    detail: format!("{label} {observed} meets {threshold}"),
                }
            } else {
                CheckOutcome::Violated {
                    detail: format!("{label} {observed} fails {threshold}"),
                }
            }
        }
        Availability::Unavailable { unavailable } => CheckOutcome::Missing {
            reason: unavailable.reason,
            detail: unavailable
                .detail
                .unwrap_or_else(|| format!("{label} unavailable")),
        },
    }
}

fn evaluate_continuity(
    profile: &SupplierProfile,
    maximum: RiskLevel,
    evidence: &mut Vec<EvidenceRef>,
    assumptions: &mut Vec<Assumption>,
) -> CheckOutcome {
    if profile.continuity.is_empty() {
        return CheckOutcome::Missing {
            reason: UnavailableReason::MissingEvidence,
            detail: "no continuity assessment".to_string(),
        };
    }
    let mut worst = None;
    for term in &profile.continuity {
        collect_basis(term, evidence, assumptions);
        match term.value.clone() {
            Availability::Known { value } => {
                if worst.is_none_or(|current: RiskLevel| value.severity > current) {
                    worst = Some(value.severity);
                }
            }
            Availability::Unavailable { unavailable } => {
                return CheckOutcome::Missing {
                    reason: unavailable.reason,
                    detail: unavailable
                        .detail
                        .unwrap_or_else(|| "continuity exposure unavailable".to_string()),
                };
            }
        }
    }
    match worst {
        Some(severity) if severity <= maximum => CheckOutcome::Satisfied {
            detail: format!("worst continuity severity {severity:?} within {maximum:?}"),
        },
        Some(severity) => CheckOutcome::Violated {
            detail: format!("worst continuity severity {severity:?} exceeds {maximum:?}"),
        },
        None => CheckOutcome::Missing {
            reason: UnavailableReason::MissingEvidence,
            detail: "no evidenced continuity exposure".to_string(),
        },
    }
}

fn collect_basis<T>(
    term: &Term<T>,
    evidence: &mut Vec<EvidenceRef>,
    assumptions: &mut Vec<Assumption>,
) {
    match term.basis() {
        Some(Basis::Provenance(provenance)) => push_evidence(evidence, provenance.evidence.clone()),
        Some(Basis::Assumption(assumption)) => {
            push_evidence(evidence, assumption.provenance.evidence.clone());
            if !assumptions
                .iter()
                .any(|existing| existing.id == assumption.id)
            {
                assumptions.push(assumption.clone());
            }
        }
        None => {}
    }
}

fn push_evidence(evidence: &mut Vec<EvidenceRef>, reference: EvidenceRef) {
    if !evidence.contains(&reference) {
        evidence.push(reference);
    }
}

fn dedup_evidence(evidence: &mut Vec<EvidenceRef>) {
    let mut seen = std::collections::HashSet::new();
    evidence.retain(|reference| seen.insert(reference.clone()));
}

fn dedup_assumptions(assumptions: &mut Vec<Assumption>) {
    assumptions.sort_by(|left, right| left.id.cmp(&right.id));
    assumptions.dedup_by(|left, right| left.id == right.id);
}

/// Group fit reports by verdict and supplier id. The order is deterministic;
/// no weighted score is invented.
pub fn rank_by_verdict(reports: &[FitReport]) -> BTreeMap<FitVerdict, Vec<SupplierId>> {
    let mut map: BTreeMap<FitVerdict, Vec<SupplierId>> = BTreeMap::new();
    for report in reports {
        map.entry(report.verdict).or_default().push(report.supplier);
    }
    for suppliers in map.values_mut() {
        suppliers.sort_unstable();
        suppliers.dedup();
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_supply_core::{Money, Provenance, Revision, SourceKind, MAX_PPM};

    const DAY: i64 = 20_000;

    fn provenance(id: &str) -> Provenance {
        Provenance::new(
            SourceKind::TestReport,
            EvidenceRef::new(id, Revision(3), Some("digest")).unwrap(),
            Day(DAY - 10),
            "fixture",
            vec!["valid for the stated revision".to_string()],
            "re-test outside tolerance",
        )
        .unwrap()
    }

    fn term<T>(value: T, id: &str) -> Term<T> {
        Term::evidence(value, provenance(id))
    }

    fn distribution(p10: i64, p50: i64, p90: i64, p99: i64) -> Distribution {
        Distribution {
            p10,
            p50,
            p90,
            p99,
            sample_size: 100,
        }
    }

    fn profile() -> SupplierProfile {
        let facts = vec![
            CapabilityFact {
                key: "smt.placement_accuracy_um".to_string(),
                value: term(
                    FactValue::Number {
                        value: 35,
                        unit: "um".to_string(),
                    },
                    "line-cap",
                ),
            },
            CapabilityFact {
                key: "smt.feeder_capacity".to_string(),
                value: term(
                    FactValue::Number {
                        value: 120,
                        unit: "feeders".to_string(),
                    },
                    "feeder-cap",
                ),
            },
            CapabilityFact {
                key: "smt.aoi".to_string(),
                value: term(FactValue::Boolean { value: true }, "aoi"),
            },
        ];
        let process = ProcessCapability {
            process: ProcessKind::Smt,
            description: "SMT line".to_string(),
            facts,
        };
        SupplierProfile {
            supplier: SupplierId(1),
            name: "Example EMS".to_string(),
            revision: Revision(4),
            authorization: AuthorizationRecord {
                role: SupplierRole::Manufacturer,
                status: AuthorizationStatus::Authorized,
                evidence: Some(EvidenceRef::new("mfr-auth", Revision(1), None).unwrap()),
                valid_until: Some(Day(DAY + 1000)),
                manufacturer: None,
            },
            processes: vec![term(process, "process")],
            equipment: vec![],
            capacity: vec![term(
                CapacityWindow {
                    site: Some(SiteId(1)),
                    process: Some(ProcessKind::Smt),
                    start: Day(DAY),
                    end: Day(DAY + 30),
                    capacity: term(Units::new(50_000).unwrap(), "capacity"),
                    committed: term(Units::new(10_000).unwrap(), "commitment"),
                },
                "capacity-window",
            )],
            lot_sizes: term(
                LotSizeRange {
                    min: Units::new(1_000).unwrap(),
                    max: Units::new(100_000).unwrap(),
                    multiple: Units::new(500).unwrap(),
                },
                "lot-size",
            ),
            tooling: vec![],
            inspection: vec![],
            process_capability: vec![ProcessCapabilityIndex {
                process: ProcessKind::Smt,
                characteristic: "solder joint height".to_string(),
                cpk: term(1_500_000, "cpk"),
                ppk: term(1_400_000, "ppk"),
                sample_size: term(125, "sample"),
                measured_on: Day(DAY - 20),
            }],
            qualifications: vec![QualificationClaim {
                kind: QualificationKind::Ppap,
                status: QualificationStatus::Approved,
                valid_until: Some(Day(DAY + 365)),
                evidence: Some(EvidenceRef::new("ppap", Revision(2), None).unwrap()),
                revision: Revision(2),
            }],
            traceability: term(TraceabilityLevel::Serial, "trace"),
            metrics: SupplierMetrics {
                otd_ppm: term(distribution(930_000, 970_000, 990_000, 1_000_000), "otd"),
                defect_ppm: term(distribution(10, 50, 120, 300), "ppm"),
                ncr_ppm: term(distribution(0, 10, 20, 40), "ncr"),
                lead_time_days: term(distribution(20, 28, 35, 45), "lead"),
                response_hours: term(distribution(2, 6, 12, 24), "response"),
            },
            continuity: vec![term(
                ContinuityExposure {
                    kind: ContinuityKind::SingleSite,
                    severity: RiskLevel::Medium,
                    recovery_days: term(30, "recovery"),
                    mitigation: term("secondary site qualified".to_string(), "mitigation"),
                },
                "continuity",
            )],
            responsiveness_hours: term(distribution(2, 6, 12, 24), "response-2"),
            ownership: Some(term(
                Ownership {
                    owner: "Example Holdings".to_string(),
                    country: "TW".to_string(),
                    parent: None,
                },
                "ownership",
            )),
            site_relationships: vec![term(
                SiteRelationship {
                    site: SiteId(1),
                    relationship: "owned plant".to_string(),
                },
                "site",
            )],
        }
    }

    fn requirement() -> SourcingRequirement {
        SourcingRequirement {
            allowed_roles: vec![SupplierRole::Manufacturer],
            required_processes: vec![ProcessKind::Smt],
            required_qualifications: vec![QualificationKind::Ppap],
            facts: vec![
                CapabilityRequirement {
                    key: "smt.placement_accuracy_um".to_string(),
                    comparison: Comparison::AtMost,
                    expected: FactValue::Number {
                        value: 50,
                        unit: "um".to_string(),
                    },
                    mandatory: true,
                },
                CapabilityRequirement {
                    key: "smt.feeder_capacity".to_string(),
                    comparison: Comparison::AtLeast,
                    expected: FactValue::Number {
                        value: 80,
                        unit: "feeders".to_string(),
                    },
                    mandatory: true,
                },
                CapabilityRequirement {
                    key: "smt.aoi".to_string(),
                    comparison: Comparison::Equals,
                    expected: FactValue::Boolean { value: true },
                    mandatory: true,
                },
            ],
            quantity: Units::new(20_000).unwrap(),
            window_start: Day(DAY + 1),
            window_end: Day(DAY + 10),
            minimum_cpk: Some(1_330_000),
            minimum_traceability: Some(TraceabilityLevel::Lot),
            maximum_defect_ppm: Some(200),
            minimum_otd_ppm: Some(950_000),
            maximum_lead_time_p90_days: Some(40),
            maximum_continuity_risk: Some(RiskLevel::High),
            maximum_response_hours: Some(12),
            as_of: Day(DAY),
        }
    }

    #[test]
    fn a_fully_evidenced_supplier_fits() {
        let report = assess(&profile(), &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::Fit);
        assert!(report.count("missing") == 0);
        assert_eq!(report.authorization.state, AuthorizationState::NotRequired);
        assert!(report.checks.iter().any(|check| {
            check.area == CheckArea::Capacity
                && matches!(check.outcome, CheckOutcome::Satisfied { .. })
        }));
    }

    #[test]
    fn missing_cpk_is_insufficient_not_fit() {
        let mut profile = profile();
        profile.process_capability.clear();
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::InsufficientEvidence);
        assert!(report.checks.iter().any(|check| {
            check.area == CheckArea::ProcessCapability
                && matches!(check.outcome, CheckOutcome::Missing { .. })
        }));
    }

    #[test]
    fn unknown_fact_is_missing_not_zero() {
        let mut profile = profile();
        if let Some(process) = profile.processes[0].value.as_known() {
            let process = process.clone();
            let mut process = process;
            process.facts[0].value = Term::unknown(
                UnavailableReason::MissingEvidence,
                "no placement accuracy report",
            );
            profile.processes[0] = term(process, "process");
        }
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::InsufficientEvidence);
    }

    #[test]
    fn conflicting_facts_block_and_are_surfaced() {
        let mut profile = profile();
        if let Some(process) = profile.processes[0].value.as_known().cloned() {
            let mut process = process;
            process.facts.push(CapabilityFact {
                key: "smt.placement_accuracy_um".to_string(),
                value: term(
                    FactValue::Number {
                        value: 80,
                        unit: "um".to_string(),
                    },
                    "old-line-cap",
                ),
            });
            profile.processes[0] = term(process, "process");
        }
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::InsufficientEvidence);
        assert!(!report.conflicts.is_empty());
    }

    #[test]
    fn unit_mismatch_cannot_satisfy_a_requirement() {
        let mut profile = profile();
        if let Some(process) = profile.processes[0].value.as_known().cloned() {
            let mut process = process;
            process.facts[0].value = term(
                FactValue::Number {
                    value: 35,
                    unit: "mil".to_string(),
                },
                "wrong-unit",
            );
            profile.processes[0] = term(process, "process");
        }
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::InsufficientEvidence);
    }

    #[test]
    fn broker_cannot_satisfy_a_manufacturer_requirement() {
        let mut profile = profile();
        profile.authorization.role = SupplierRole::Broker;
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::NotFit);
    }

    #[test]
    fn authorized_distributor_without_evidence_is_refused_at_validation() {
        let mut profile = profile();
        profile.authorization.role = SupplierRole::AuthorizedDistributor;
        profile.authorization.status = AuthorizationStatus::Authorized;
        profile.authorization.evidence = None;
        // Breaks if an authorized role can exist without evidence.
        assert!(matches!(
            profile.validate(),
            Err(ExactError::OutOfBounds {
                what: "authorization",
                ..
            })
        ));
    }

    #[test]
    fn expired_authorization_is_insufficient() {
        let mut profile = profile();
        profile.authorization.role = SupplierRole::AuthorizedDistributor;
        profile.authorization.status = AuthorizationStatus::Authorized;
        profile.authorization.evidence = Some(EvidenceRef::new("auth", Revision(1), None).unwrap());
        profile.authorization.valid_until = Some(Day(DAY - 1));
        let mut requirement = requirement();
        requirement.allowed_roles = vec![SupplierRole::AuthorizedDistributor];
        let report = assess(&profile, &requirement).unwrap();
        assert_eq!(report.verdict, FitVerdict::InsufficientEvidence);
        assert_eq!(report.authorization.state, AuthorizationState::Expired);
    }

    #[test]
    fn fake_approved_qualification_is_refused() {
        let mut profile = profile();
        profile.qualifications[0].evidence = None;
        // Breaks if an approved qualification can be claimed without evidence.
        assert!(matches!(
            profile.validate(),
            Err(ExactError::OutOfBounds {
                what: "qualification",
                ..
            })
        ));
    }

    #[test]
    fn expired_qualification_is_typed() {
        let mut profile = profile();
        profile.qualifications[0].valid_until = Some(Day(DAY - 2));
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::InsufficientEvidence);
        assert!(report.checks.iter().any(|check| {
            check.area == CheckArea::Qualification
                && matches!(
                    &check.outcome,
                    CheckOutcome::Missing {
                        reason: UnavailableReason::Expired,
                        ..
                    }
                )
        }));
    }

    #[test]
    fn capacity_must_cover_the_window_and_quantity() {
        let mut requirement = requirement();
        requirement.quantity = Units::new(90_000).unwrap();
        let report = assess(&profile(), &requirement).unwrap();
        assert_eq!(report.verdict, FitVerdict::NotFit);
        let mut late = requirement.clone();
        late.quantity = Units::new(1_000).unwrap();
        late.window_start = Day(DAY + 40);
        late.window_end = Day(DAY + 45);
        let report = assess(&profile(), &late).unwrap();
        assert_eq!(report.verdict, FitVerdict::InsufficientEvidence);
    }

    #[test]
    fn low_otd_and_high_ppm_fail() {
        let mut requirement = requirement();
        requirement.minimum_otd_ppm = Some(990_000);
        requirement.maximum_defect_ppm = Some(10);
        let report = assess(&profile(), &requirement).unwrap();
        assert_eq!(report.verdict, FitVerdict::NotFit);
    }

    #[test]
    fn continuity_risk_above_limit_fails() {
        let mut requirement = requirement();
        requirement.maximum_continuity_risk = Some(RiskLevel::Low);
        let report = assess(&profile(), &requirement).unwrap();
        assert_eq!(report.verdict, FitVerdict::NotFit);
    }

    #[test]
    fn rank_by_verdict_is_deterministic_and_does_not_score() {
        let mut second = profile();
        second.supplier = SupplierId(2);
        second.name = "Second".to_string();
        let first_report = assess(&profile(), &requirement()).unwrap();
        let second_report = assess(&second, &requirement()).unwrap();
        let map = rank_by_verdict(&[second_report, first_report]);
        assert_eq!(map[&FitVerdict::Fit], vec![SupplierId(1), SupplierId(2)]);
    }

    #[test]
    fn assumptions_are_collected_not_laundered() {
        let mut profile = profile();
        let assumption = Assumption::new(
            "a-capacity",
            "committed load will not change before the window",
            "no new orders are visible",
            provenance("capacity-assumption"),
            "a new booking consuming the remaining capacity",
        )
        .unwrap();
        if let Some(window) = profile.capacity[0].value.as_known().cloned() {
            let mut window = window;
            window.committed = Term::assumption(Units::new(10_000).unwrap(), assumption);
            profile.capacity[0] = term(window, "capacity-window");
        }
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::Fit);
        assert_eq!(report.assumptions.len(), 1);
        assert!(report
            .assumptions
            .iter()
            .any(|assumption| assumption.id == "a-capacity"));
    }

    #[test]
    fn roles_and_envelopes_are_bounded() {
        let mut requirement = requirement();
        requirement.allowed_roles.clear();
        assert!(requirement.validate().is_err());
        let mut bad_lot = LotSizeRange {
            min: Units::new(10).unwrap(),
            max: Units::new(5).unwrap(),
            multiple: Units::new(1).unwrap(),
        };
        assert!(bad_lot.validate().is_err());
        bad_lot.min = Units::new(1).unwrap();
        bad_lot.max = Units::new(5).unwrap();
        bad_lot.multiple = Units::ZERO;
        assert!(bad_lot.validate().is_err());
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FitFixture {
        profile: SupplierProfile,
        requirement: SourcingRequirement,
    }

    #[test]
    fn json_fixture_evaluates_a_manufacturer() {
        let fixture: FitFixture =
            serde_json::from_str(include_str!("../tests/fixtures/supplier_fit.json")).unwrap();
        let report = assess(&fixture.profile, &fixture.requirement).unwrap();
        assert_eq!(report.verdict, FitVerdict::Fit);
        assert!(report.count("violated") == 0);
    }

    #[test]
    fn ten_thousand_supplier_assessments_are_bounded() {
        let base = profile();
        let requirement = requirement();
        let started = std::time::Instant::now();
        for index in 0..10_000_u32 {
            let mut candidate = base.clone();
            candidate.supplier = SupplierId(index);
            candidate.name = format!("supplier {index}");
            let report = assess(&candidate, &requirement).unwrap();
            assert_eq!(report.verdict, FitVerdict::Fit);
        }
        assert!(
            started.elapsed().as_secs() < 30,
            "supplier assessments took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn fact_index_preserves_cross_area_conflicts() {
        let mut profile = profile();
        let mut requirement = requirement();
        requirement.facts = vec![CapabilityRequirement {
            key: "smt.placement_accuracy_um".to_string(),
            comparison: Comparison::AtMost,
            expected: FactValue::Number {
                value: 50,
                unit: "um".to_string(),
            },
            mandatory: true,
        }];
        let low = FactValue::Number {
            value: 40,
            unit: "um".to_string(),
        };
        let high = FactValue::Number {
            value: 60,
            unit: "um".to_string(),
        };
        if let Some(process) = profile.processes[0].value.as_known().cloned() {
            let mut process = process;
            process.facts = vec![CapabilityFact {
                key: "smt.placement_accuracy_um".to_string(),
                value: term(low, "equipment-fact"),
            }];
            profile.processes[0] = term(process, "process");
        }
        let mut second = ProcessCapability {
            process: ProcessKind::Smt,
            description: "second SMT line".to_string(),
            facts: vec![CapabilityFact {
                key: "smt.placement_accuracy_um".to_string(),
                value: term(high, "process-fact"),
            }],
        };
        second.facts.push(CapabilityFact {
            key: "smt.aoi".to_string(),
            value: term(FactValue::Boolean { value: true }, "second-aoi"),
        });
        profile.processes.push(term(second, "second-process"));
        let report = assess(&profile, &requirement).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.area == CheckArea::Capability)
            .expect("capability check");
        match &check.outcome {
            CheckOutcome::Conflicting { values } => {
                assert_eq!(values, &vec!["40 um".to_string(), "60 um".to_string()]);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "[perf] supplier matching at 10k suppliers, 128 facts, 128 requirements"]
    fn perf_supplier_matching_is_index_bound() {
        let mut base = profile();
        if let Some(process) = base.processes[0].value.as_known().cloned() {
            let mut process = process;
            process.facts = (0..128)
                .map(|index| CapabilityFact {
                    key: format!("perf.key.{index}"),
                    value: term(
                        FactValue::Number {
                            value: 1,
                            unit: "um".to_string(),
                        },
                        "perf-fact",
                    ),
                })
                .collect();
            base.processes[0] = term(process, "perf-process");
        }
        let mut requirement = requirement();
        requirement.facts = (0..126)
            .map(|index| CapabilityRequirement {
                key: format!("perf.key.{index}"),
                comparison: Comparison::AtMost,
                expected: FactValue::Number {
                    value: 10,
                    unit: "um".to_string(),
                },
                mandatory: false,
            })
            .collect();
        let started = std::time::Instant::now();
        for index in 0..10_000_u32 {
            let mut candidate = base.clone();
            candidate.supplier = SupplierId(index);
            candidate.name = format!("supplier {index}");
            let report = assess(&candidate, &requirement).unwrap();
            assert_eq!(report.verdict, FitVerdict::Fit);
        }
        let elapsed = started.elapsed();
        eprintln!(
            "perf supplier: 10k suppliers x 128 facts x 128 requirements = {:?} ({:?}/supplier)",
            elapsed,
            elapsed / 10_000
        );
        assert!(
            elapsed.as_secs() < 10,
            "indexed assessment took {elapsed:?}"
        );
    }

    #[test]
    fn money_term_is_supported_for_tooling() {
        let mut profile = profile();
        profile.tooling = vec![term(
            ToolingCapability {
                kind: "SMT stencil".to_string(),
                in_house: term(true, "in-house"),
                lead_time_days: term(5, "tooling-lead"),
                cost: term(
                    Money::parse("1200.00", faktor_supply_core::Currency::USD).unwrap(),
                    "tooling-cost",
                ),
            },
            "tooling",
        )];
        let report = assess(&profile, &requirement()).unwrap();
        assert_eq!(report.verdict, FitVerdict::Fit);
        // Breaks if a tooling money term is dropped or truncated to the wrong unit.
        let tooling = report
            .checks
            .iter()
            .find(|check| check.area == CheckArea::Tooling)
            .expect("tooling check");
        assert!(
            matches!(&tooling.outcome, CheckOutcome::Satisfied { detail }
                if detail == "1 tooling records evidenced"),
            "{:?}",
            tooling.outcome
        );
        assert_eq!(
            profile.tooling[0]
                .as_known()
                .unwrap()
                .cost
                .as_known()
                .unwrap()
                .micros(),
            1_200_000_000
        );

        let mut requirement = requirement();
        requirement.minimum_otd_ppm = Some(MAX_PPM);
        assert!(requirement.validate().is_ok());
    }
}
