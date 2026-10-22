//! faktor-supply-quality — evidence-bound qualification.
//!
//! A qualification state is **derived**, never assigned. This crate models the
//! PPAP / APQP / FAI / NCR vocabulary and evaluates mandatory gates against
//! revision-bound evidence. The program-fact laws are structural:
//!
//! - an `Approved` NCR closure without containment and corrective-action
//!   evidence cannot be constructed (`validate` refuses it);
//! - a qualification approval is bound to the record revision; an approval of
//!   an older revision can never approve the current one;
//! - a PPAP submission below the required level, an incomplete FAI, an MSA
//!   outside its gauge-R&R limit, a Cpk/Ppk below the gate, an open NCR, an
//!   open audit finding or an unacknowledged PCN blocks the gate;
//! - missing evidence produces `evidence_pending`; contradictory evidence
//!   produces a surfaced conflict and blocks, it never averages;
//! - `Qualified` additionally requires an evidenced approval for the current
//!   revision. Evidence alone is not approval.

use std::collections::HashSet;

use faktor_supply_core::{
    validate_text, Day, EvidenceRef, ExactError, Revision, MAX_ID_BYTES, MAX_TEXT_BYTES,
};
use serde::{Deserialize, Serialize};

/// Maximum gate requirements in one qualification.
pub const MAX_GATES: usize = 64;
/// Maximum evidence records.
pub const MAX_EVIDENCE_RECORDS: usize = 128;
/// Maximum NCRs.
pub const MAX_NCRS: usize = 256;
/// Maximum approvals.
pub const MAX_APPROVALS: usize = 8;
/// Maximum PPAP elements.
pub const MAX_PPAP_ELEMENTS: usize = 32;
/// Maximum APQP phases.
pub const MAX_APQP_PHASES: usize = 8;
/// Maximum NCR ids on one gate.
pub const MAX_NCR_IDS: usize = 64;
/// Maximum approval conditions.
pub const MAX_CONDITIONS: usize = 16;

// -------------------------------------------------------------- qualification

/// Qualification lifecycle state. Derived by [`assess`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualificationState {
    /// No evidence or gates exist.
    NotStarted,
    /// Gates or approval are incomplete.
    EvidencePending,
    /// All mandatory gates and an evidenced approval are current.
    Qualified,
    /// Qualified subject to explicit conditions.
    Conditional,
    /// Suspended by an evidenced authority.
    Suspended,
    /// Approval or validity elapsed.
    Expired,
    /// A mandatory quality gate failed.
    Disqualified,
}

impl QualificationState {
    /// Stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::EvidencePending => "evidence_pending",
            Self::Qualified => "qualified",
            Self::Conditional => "conditional",
            Self::Suspended => "suspended",
            Self::Expired => "expired",
            Self::Disqualified => "disqualified",
        }
    }
}

/// Explicit state-transition matrix. Transitions not listed are illegal.
pub fn can_transition(from: QualificationState, to: QualificationState) -> bool {
    use QualificationState::{
        Conditional, Disqualified, EvidencePending, Expired, NotStarted, Qualified, Suspended,
    };
    match from {
        NotStarted => matches!(to, EvidencePending | Disqualified),
        EvidencePending => matches!(to, Qualified | Conditional | Disqualified | Expired),
        Qualified => matches!(to, Suspended | Expired | Disqualified | Conditional),
        Conditional => matches!(to, Qualified | Suspended | Expired | Disqualified),
        Suspended => matches!(to, EvidencePending | Disqualified | Expired),
        Expired => matches!(to, EvidencePending | Disqualified),
        Disqualified => matches!(to, EvidencePending),
    }
}

/// PPAP submission level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PpapLevel {
    /// Level 1: PSW only.
    Level1,
    /// Level 2: PSW, samples and limited supporting data.
    Level2,
    /// Level 3: PSW, samples and full supporting data.
    Level3,
    /// Level 4: PSW plus customer-defined requirements.
    Level4,
    /// Level 5: PSW, samples, full data and on-site review.
    Level5,
}

impl PpapLevel {
    /// Numeric level.
    pub const fn number(self) -> u8 {
        match self {
            Self::Level1 => 1,
            Self::Level2 => 2,
            Self::Level3 => 3,
            Self::Level4 => 4,
            Self::Level5 => 5,
        }
    }

    /// The modeled element set for a level. Level 4 is customer-defined and
    /// returns an empty set; the gate then requires an explicit element list.
    pub fn modeled_elements(self) -> Vec<PpapElement> {
        use PpapElement::{
            AppearanceApproval, AuthorizedEngineeringChange, CheckingAids, ControlPlan,
            CustomerEngineeringApproval, CustomerSpecific, DesignRecords, Dfmea,
            DimensionalResults, InitialProcessStudies, MasterSample, MaterialPerformance, Msa,
            OnSiteReview, Pfmea, ProcessFlow, Psw, QualifiedLab, SampleParts,
        };
        match self {
            Self::Level1 => vec![Psw],
            Self::Level2 => vec![
                Psw,
                SampleParts,
                DimensionalResults,
                MaterialPerformance,
                InitialProcessStudies,
            ],
            Self::Level3 | Self::Level5 => {
                let mut elements = vec![
                    Psw,
                    DesignRecords,
                    AuthorizedEngineeringChange,
                    CustomerEngineeringApproval,
                    ProcessFlow,
                    Dfmea,
                    Msa,
                    DimensionalResults,
                    MaterialPerformance,
                    InitialProcessStudies,
                    QualifiedLab,
                    SampleParts,
                    MasterSample,
                    CheckingAids,
                    CustomerSpecific,
                    AppearanceApproval,
                    ControlPlan,
                    Pfmea,
                ];
                if self == Self::Level5 {
                    elements.push(OnSiteReview);
                }
                elements
            }
            Self::Level4 => Vec::new(),
        }
    }
}

/// PPAP element vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PpapElement {
    /// Design records.
    DesignRecords,
    /// Authorized engineering change documents.
    AuthorizedEngineeringChange,
    /// Customer engineering approval.
    CustomerEngineeringApproval,
    /// Design FMEA.
    Dfmea,
    /// Process flow diagram.
    ProcessFlow,
    /// Process FMEA.
    Pfmea,
    /// Control plan.
    ControlPlan,
    /// Measurement system analysis.
    Msa,
    /// Dimensional results.
    DimensionalResults,
    /// Material / performance test results.
    MaterialPerformance,
    /// Initial process studies.
    InitialProcessStudies,
    /// Qualified laboratory documentation.
    QualifiedLab,
    /// Appearance approval report.
    AppearanceApproval,
    /// Sample parts.
    SampleParts,
    /// Master sample.
    MasterSample,
    /// Checking aids.
    CheckingAids,
    /// Customer-specific requirements.
    CustomerSpecific,
    /// Part submission warrant.
    Psw,
    /// On-site review (level 5).
    OnSiteReview,
}

/// APQP phase vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApqpPhase {
    /// Plan and define programme.
    Plan,
    /// Product design and development.
    ProductDesign,
    /// Process design and development.
    ProcessDesign,
    /// Product and process validation.
    Validation,
    /// Launch, feedback, assessment and corrective action.
    Production,
}

/// A PPAP submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PpapSubmission {
    /// Declared submission level.
    pub level: PpapLevel,
    /// Part submission warrant signed by an authorized supplier signatory.
    pub psw_signed: bool,
    /// Submitted elements.
    #[serde(default)]
    pub elements: Vec<PpapElement>,
    /// Submission day.
    pub submitted_on: Day,
}

impl PpapSubmission {
    fn validate(&self) -> Result<(), ExactError> {
        if self.elements.len() > MAX_PPAP_ELEMENTS {
            return Err(ExactError::OutOfBounds {
                what: "PPAP elements",
                detail: format!("{} entries", self.elements.len()),
            });
        }
        Ok(())
    }
}

/// An APQP submission record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApqpSubmission {
    /// Completed phases.
    #[serde(default)]
    pub completed_phases: Vec<ApqpPhase>,
}

impl ApqpSubmission {
    fn validate(&self) -> Result<(), ExactError> {
        if self.completed_phases.len() > MAX_APQP_PHASES {
            return Err(ExactError::OutOfBounds {
                what: "APQP phases",
                detail: format!("{} entries", self.completed_phases.len()),
            });
        }
        Ok(())
    }
}

/// A First Article Inspection report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaiReport {
    /// Characteristics on the ballooned drawing.
    pub characteristics_total: u32,
    /// Characteristics verified.
    pub characteristics_verified: u32,
    /// Open nonconformances.
    pub open_nonconformances: u32,
    /// Drawing ballooned and traceable.
    pub ballooned_drawing: bool,
    /// Inspection day.
    pub performed_on: Day,
}

impl FaiReport {
    fn validate(&self) -> Result<(), ExactError> {
        if self.characteristics_total == 0 {
            return Err(ExactError::OutOfBounds {
                what: "FAI",
                detail: "zero characteristics".to_string(),
            });
        }
        if self.characteristics_verified > self.characteristics_total {
            return Err(ExactError::OutOfBounds {
                what: "FAI",
                detail: "verified exceeds total".to_string(),
            });
        }
        Ok(())
    }
}

/// A supplier audit report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditReport {
    /// Open findings.
    pub open_findings: u32,
    /// Open critical findings.
    pub critical_findings: u32,
    /// Audit day.
    pub audited_on: Day,
}

/// An MSA / gauge R&R report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MsaReport {
    /// Gauge R&R as percent of tolerance, in ppm.
    pub grr_ppm: i64,
    /// Study sample size.
    pub sample_size: u32,
    /// Accepted by the quality authority (in addition to the numeric gate).
    pub accepted_by_authority: bool,
}

/// A process-capability submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitySubmission {
    /// Cpk scaled by 1e6.
    pub cpk: i64,
    /// Ppk scaled by 1e6.
    pub ppk: i64,
    /// Sample size.
    pub sample_size: u32,
    /// Measurement day.
    pub measured_on: Day,
}

/// Traceability depth vocabulary (quality view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceabilityDepth {
    /// None.
    None,
    /// Lot/batch.
    Lot,
    /// Serial number.
    Serial,
    /// Full as-built genealogy.
    FullGenealogy,
}

/// A traceability submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceabilitySubmission {
    /// Achieved depth.
    pub depth: TraceabilityDepth,
    /// Production records are retrievable for the required retention period.
    pub records_available: bool,
}

/// A PCN review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PcnReview {
    /// PCN identifier.
    pub pcn_id: String,
    /// Supplier acknowledgement.
    pub acknowledged: bool,
    /// Impact assessment completed.
    pub impact_assessed: bool,
}

impl PcnReview {
    fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.pcn_id, MAX_ID_BYTES, "PCN id")
    }
}

/// The gate requirement vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "gate", rename_all = "snake_case", deny_unknown_fields)]
pub enum GateSpec {
    /// PPAP at or above a level, with a signed PSW and all modeled elements.
    Ppap {
        /// Minimum level.
        min_level: PpapLevel,
        /// Require a signed PSW.
        require_psw: bool,
        /// Explicit required elements (used for level 4 or to tighten a level).
        #[serde(default)]
        required_elements: Vec<PpapElement>,
    },
    /// APQP phase completion.
    Apqp {
        /// Required completed phase.
        required_phase: ApqpPhase,
    },
    /// First Article Inspection.
    Fai,
    /// Closure of a specific NCR set.
    NcrClosure {
        /// The NCR ids that must be resolved.
        ncr_ids: Vec<String>,
    },
    /// Supplier audit.
    Audit {
        /// Maximum open findings.
        max_open_findings: u32,
        /// Maximum open critical findings.
        max_critical_findings: u32,
    },
    /// Measurement system analysis.
    Msa {
        /// Maximum gauge R&R in ppm of tolerance.
        max_grr_ppm: i64,
        /// Minimum study sample.
        min_sample: u32,
    },
    /// Process capability.
    ProcessCapability {
        /// Minimum Cpk (scaled 1e6).
        min_cpk: i64,
        /// Minimum Ppk (scaled 1e6).
        min_ppk: i64,
        /// Minimum sample.
        min_sample: u32,
    },
    /// Traceability depth.
    Traceability {
        /// Minimum depth.
        min_depth: TraceabilityDepth,
    },
    /// PCN review.
    PcnReview {
        /// The PCN to review.
        pcn_id: String,
    },
}

impl GateSpec {
    fn label(&self) -> &'static str {
        match self {
            Self::Ppap { .. } => "ppap",
            Self::Apqp { .. } => "apqp",
            Self::Fai => "fai",
            Self::NcrClosure { .. } => "ncr_closure",
            Self::Audit { .. } => "audit",
            Self::Msa { .. } => "msa",
            Self::ProcessCapability { .. } => "process_capability",
            Self::Traceability { .. } => "traceability",
            Self::PcnReview { .. } => "pcn_review",
        }
    }
}

/// One qualification gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRequirement {
    /// Stable gate id.
    pub id: String,
    /// The requirement.
    pub spec: GateSpec,
    /// Mandatory gates block qualification.
    pub mandatory: bool,
    /// Maximum age of the evidence in days.
    pub max_age_days: Option<u32>,
}

impl GateRequirement {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.id, MAX_ID_BYTES, "gate id")?;
        match &self.spec {
            GateSpec::Ppap {
                required_elements, ..
            } => {
                if required_elements.len() > MAX_PPAP_ELEMENTS {
                    return Err(ExactError::OutOfBounds {
                        what: "PPAP required elements",
                        detail: format!("{} entries", required_elements.len()),
                    });
                }
            }
            GateSpec::NcrClosure { ncr_ids } => {
                if ncr_ids.is_empty() || ncr_ids.len() > MAX_NCR_IDS {
                    return Err(ExactError::OutOfBounds {
                        what: "NCR ids",
                        detail: format!("{} entries", ncr_ids.len()),
                    });
                }
                for id in ncr_ids {
                    validate_text(id, MAX_ID_BYTES, "NCR id")?;
                }
            }
            GateSpec::Msa {
                max_grr_ppm,
                min_sample,
            } => {
                if *max_grr_ppm < 0 {
                    return Err(ExactError::Negative {
                        what: "maximum gauge R&R",
                    });
                }
                if *min_sample == 0 {
                    return Err(ExactError::OutOfBounds {
                        what: "MSA sample",
                        detail: "zero".to_string(),
                    });
                }
            }
            GateSpec::ProcessCapability {
                min_cpk,
                min_ppk,
                min_sample,
            } => {
                if *min_sample == 0 {
                    return Err(ExactError::OutOfBounds {
                        what: "capability sample",
                        detail: "zero".to_string(),
                    });
                }
                if *min_cpk <= 0 || *min_ppk <= 0 {
                    return Err(ExactError::OutOfBounds {
                        what: "capability index",
                        detail: "must be positive".to_string(),
                    });
                }
            }
            GateSpec::PcnReview { pcn_id } => validate_text(pcn_id, MAX_ID_BYTES, "PCN id")?,
            GateSpec::Apqp { .. }
            | GateSpec::Fai
            | GateSpec::Audit { .. }
            | GateSpec::Traceability { .. } => {}
        }
        Ok(())
    }
}

/// The typed evidence payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidencePayload {
    /// PPAP submission.
    Ppap(PpapSubmission),
    /// APQP submission.
    Apqp(ApqpSubmission),
    /// FAI report.
    Fai(FaiReport),
    /// Audit report.
    Audit(AuditReport),
    /// MSA report.
    Msa(MsaReport),
    /// Capability submission.
    ProcessCapability(CapabilitySubmission),
    /// Traceability submission.
    Traceability(TraceabilitySubmission),
    /// PCN review.
    PcnReview(PcnReview),
}

/// One revision-bound evidence record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateEvidence {
    /// Stable evidence record id.
    pub id: String,
    /// Revision-bound evidence reference.
    pub evidence: EvidenceRef,
    /// The qualification revision this evidence applies to.
    pub bound_revision: Revision,
    /// Capture day.
    pub captured_on: Day,
    /// Expiry day.
    pub valid_until: Option<Day>,
    /// The payload.
    pub payload: EvidencePayload,
}

impl GateEvidence {
    /// Validate bounds and the payload.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.id, MAX_ID_BYTES, "evidence id")?;
        self.evidence.validate()?;
        match &self.payload {
            EvidencePayload::Ppap(payload) => payload.validate(),
            EvidencePayload::Apqp(payload) => payload.validate(),
            EvidencePayload::Fai(payload) => payload.validate(),
            EvidencePayload::Audit(_) => Ok(()),
            EvidencePayload::Msa(payload) => {
                if payload.sample_size == 0 {
                    return Err(ExactError::OutOfBounds {
                        what: "MSA sample",
                        detail: "zero".to_string(),
                    });
                }
                if payload.grr_ppm < 0 {
                    return Err(ExactError::Negative { what: "gauge R&R" });
                }
                Ok(())
            }
            EvidencePayload::ProcessCapability(payload) => {
                if payload.sample_size == 0 {
                    return Err(ExactError::OutOfBounds {
                        what: "capability sample",
                        detail: "zero".to_string(),
                    });
                }
                Ok(())
            }
            EvidencePayload::Traceability(_) => Ok(()),
            EvidencePayload::PcnReview(payload) => payload.validate(),
        }
    }

    fn kind_label(&self) -> &'static str {
        match self.payload {
            EvidencePayload::Ppap(_) => "ppap",
            EvidencePayload::Apqp(_) => "apqp",
            EvidencePayload::Fai(_) => "fai",
            EvidencePayload::Audit(_) => "audit",
            EvidencePayload::Msa(_) => "msa",
            EvidencePayload::ProcessCapability(_) => "process_capability",
            EvidencePayload::Traceability(_) => "traceability",
            EvidencePayload::PcnReview(_) => "pcn_review",
        }
    }
}

/// NCR severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcrSeverity {
    /// Minor.
    Minor,
    /// Major.
    Major,
    /// Critical.
    Critical,
}

/// NCR disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcrDisposition {
    /// Use as is.
    UseAsIs,
    /// Rework.
    Rework,
    /// Repair.
    Repair,
    /// Scrap.
    Scrap,
    /// Return to supplier.
    ReturnToSupplier,
}

/// NCR lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcrStatus {
    /// Open, no containment.
    Open,
    /// Contained.
    Contained,
    /// Corrective action in progress.
    CorrectiveActionPending,
    /// Closed with verified containment and corrective action.
    Closed,
    /// Voided as invalid.
    Void,
}

/// One nonconformance report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NcrRecord {
    /// NCR identifier.
    pub id: String,
    /// Severity.
    pub severity: NcrSeverity,
    /// Status.
    pub status: NcrStatus,
    /// Disposition.
    pub disposition: NcrDisposition,
    /// Containment evidence.
    pub containment_evidence: Option<EvidenceRef>,
    /// Corrective-action evidence.
    pub corrective_action_evidence: Option<EvidenceRef>,
    /// Opened day.
    pub opened_on: Day,
    /// Closed day.
    pub closed_on: Option<Day>,
    /// Void reason (required when voided).
    pub void_reason: Option<String>,
}

impl NcrRecord {
    /// Validate the evidence-bound closure law.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.id, MAX_ID_BYTES, "NCR id")?;
        if let Some(evidence) = &self.containment_evidence {
            evidence.validate()?;
        }
        if let Some(evidence) = &self.corrective_action_evidence {
            evidence.validate()?;
        }
        if self.status == NcrStatus::Closed {
            if self.containment_evidence.is_none() {
                return Err(ExactError::OutOfBounds {
                    what: "NCR",
                    detail: "closed without containment evidence".to_string(),
                });
            }
            if self.corrective_action_evidence.is_none() {
                return Err(ExactError::OutOfBounds {
                    what: "NCR",
                    detail: "closed without corrective-action evidence".to_string(),
                });
            }
            if self.closed_on.is_none() {
                return Err(ExactError::OutOfBounds {
                    what: "NCR",
                    detail: "closed without a closure day".to_string(),
                });
            }
        }
        if self.status == NcrStatus::Void {
            let reason = self.void_reason.as_deref().unwrap_or("");
            validate_text(reason, MAX_TEXT_BYTES, "NCR void reason")?;
        }
        if self.status == NcrStatus::Contained && self.containment_evidence.is_none() {
            return Err(ExactError::OutOfBounds {
                what: "NCR",
                detail: "contained without containment evidence".to_string(),
            });
        }
        if self.status == NcrStatus::CorrectiveActionPending
            && self.corrective_action_evidence.is_none()
        {
            return Err(ExactError::OutOfBounds {
                what: "NCR",
                detail: "corrective action claimed without evidence".to_string(),
            });
        }
        Ok(())
    }
}

/// A revision-bound approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// The approving authority.
    pub authority: String,
    /// Signature / approval evidence.
    pub evidence: EvidenceRef,
    /// The qualification revision approved.
    pub approved_revision: Revision,
    /// Approval day.
    pub approved_on: Day,
    /// Approval validity end.
    pub valid_until: Option<Day>,
    /// Conditions attached (non-empty makes the state `conditional`).
    #[serde(default)]
    pub conditions: Vec<String>,
}

impl Approval {
    /// Validate the approval bound and its conditions.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.authority, MAX_ID_BYTES, "approval authority")?;
        self.evidence.validate()?;
        if self.conditions.len() > MAX_CONDITIONS {
            return Err(ExactError::OutOfBounds {
                what: "approval conditions",
                detail: format!("{} entries", self.conditions.len()),
            });
        }
        for condition in &self.conditions {
            validate_text(condition, MAX_TEXT_BYTES, "approval condition")?;
        }
        Ok(())
    }
}

/// A recorded suspension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suspension {
    /// Reason.
    pub reason: String,
    /// Evidence.
    pub evidence: EvidenceRef,
    /// Suspension end day; `None` means indefinite.
    pub until: Option<Day>,
}

impl Suspension {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.reason, MAX_TEXT_BYTES, "suspension reason")?;
        self.evidence.validate()
    }
}

/// The qualification record. It has no editable state field by design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    /// Stable qualification id.
    pub id: String,
    /// Subject description (supplier/site/part/process).
    pub subject: String,
    /// Revision of the qualified subject.
    pub revision: Revision,
    /// Overall validity end.
    pub valid_until: Option<Day>,
    /// Gates.
    pub requirements: Vec<GateRequirement>,
    /// Evidence records.
    #[serde(default)]
    pub evidence: Vec<GateEvidence>,
    /// NCRs.
    #[serde(default)]
    pub ncrs: Vec<NcrRecord>,
    /// Approvals.
    #[serde(default)]
    pub approvals: Vec<Approval>,
    /// Active or historical suspension record.
    pub suspension: Option<Suspension>,
}

impl Qualification {
    /// Validate every bound and every evidence law.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.id, MAX_ID_BYTES, "qualification id")?;
        validate_text(&self.subject, MAX_TEXT_BYTES, "qualification subject")?;
        if self.requirements.len() > MAX_GATES {
            return Err(ExactError::OutOfBounds {
                what: "gates",
                detail: format!("{} entries", self.requirements.len()),
            });
        }
        let mut gate_ids = HashSet::new();
        for gate in &self.requirements {
            gate.validate()?;
            if !gate_ids.insert(gate.id.clone()) {
                return Err(ExactError::OutOfBounds {
                    what: "gate ids",
                    detail: format!("duplicate {}", gate.id),
                });
            }
        }
        if self.evidence.len() > MAX_EVIDENCE_RECORDS {
            return Err(ExactError::OutOfBounds {
                what: "evidence records",
                detail: format!("{} entries", self.evidence.len()),
            });
        }
        for evidence in &self.evidence {
            evidence.validate()?;
        }
        if self.ncrs.len() > MAX_NCRS {
            return Err(ExactError::OutOfBounds {
                what: "NCRs",
                detail: format!("{} entries", self.ncrs.len()),
            });
        }
        let mut ncr_ids = HashSet::new();
        for ncr in &self.ncrs {
            ncr.validate()?;
            if !ncr_ids.insert(ncr.id.clone()) {
                return Err(ExactError::OutOfBounds {
                    what: "NCR ids",
                    detail: format!("duplicate {}", ncr.id),
                });
            }
        }
        if self.approvals.len() > MAX_APPROVALS {
            return Err(ExactError::OutOfBounds {
                what: "approvals",
                detail: format!("{} entries", self.approvals.len()),
            });
        }
        for approval in &self.approvals {
            approval.validate()?;
        }
        if let Some(suspension) = &self.suspension {
            suspension.validate()?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- assessment

/// The outcome of one gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GateOutcome {
    /// The gate is met.
    Satisfied {
        /// Explanation.
        detail: String,
    },
    /// Evidence is present but incomplete, or missing.
    Pending {
        /// Explanation.
        detail: String,
    },
    /// Evidence proves the gate is failed.
    Failed {
        /// Explanation.
        detail: String,
    },
    /// Evidence of one gate conflicts.
    Conflicting {
        /// The competing claims.
        values: Vec<String>,
    },
}

/// One evaluated gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateEvaluation {
    /// Gate id.
    pub id: String,
    /// Gate class.
    pub gate: String,
    /// Mandatory.
    pub mandatory: bool,
    /// Outcome.
    pub outcome: GateOutcome,
    /// Evidence used.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// How the approval was judged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApprovalAssessment {
    /// No approval exists.
    Absent,
    /// A current approval for the record revision.
    Valid {
        /// Authority.
        authority: String,
        /// Whether conditions were attached.
        conditions: Vec<String>,
    },
    /// An approval exists but for a different revision.
    StaleRevision {
        /// The approved revision.
        approved_revision: Revision,
        /// The current revision.
        current_revision: Revision,
    },
    /// An approval exists but is outside its validity.
    Expired {
        /// The validity end.
        valid_until: Day,
    },
    /// An approval is dated after the assessment day.
    FutureDated {
        /// The approval day.
        approved_on: Day,
    },
    /// Multiple approvals disagree on conditions.
    Conflicting {
        /// Competing descriptions.
        values: Vec<String>,
    },
}

/// The complete assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationAssessment {
    /// Qualification id.
    pub id: String,
    /// Subject.
    pub subject: String,
    /// Assessment day.
    pub as_of: Day,
    /// Record revision.
    pub revision: Revision,
    /// Derived state.
    pub state: QualificationState,
    /// Approval assessment.
    pub approval: ApprovalAssessment,
    /// Gates.
    pub gates: Vec<GateEvaluation>,
    /// Blocking reasons (bounded by the gate/approval count).
    #[serde(default)]
    pub blockers: Vec<String>,
    /// Evidence referenced.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// The exact digest a human qualification approval binds to.
pub fn qualification_digest(qualification: &Qualification) -> Result<String, ExactError> {
    faktor_approval::content_digest(b"faktor.supply.qualification/v1", qualification)
        .map_err(ExactError::Approval)
}

/// The exact check a human qualification approval must satisfy.
pub fn qualification_approval_check(
    qualification: &Qualification,
) -> Result<faktor_approval::ApprovalCheck, ExactError> {
    Ok(faktor_approval::ApprovalCheck::new(
        faktor_approval::ApprovalKind::SupplySupplierQualification,
        format!("qualification:{}", qualification.id),
        qualification.revision.get().to_string(),
        qualification_digest(qualification)?,
    ))
}

/// Assess a qualification at a day. The state is derived from evidence; an
/// approval or a state string cannot override a failed or missing gate. This
/// entry point has no human approval, so a `qualified`/`conditional` result
/// is refused typed. Use [`assess_with_approval`] for the action path.
pub fn assess(
    qualification: &Qualification,
    as_of: Day,
) -> Result<QualificationAssessment, ExactError> {
    assess_with_approval(qualification, as_of, None)
}

/// Assess a qualification with an optional human approval token. A token is
/// required exactly when the derived state would be `qualified` or
/// `conditional`; it must bind to the qualification revision and digest.
pub fn assess_with_approval(
    qualification: &Qualification,
    as_of: Day,
    approval_token: Option<&faktor_approval::ApprovalToken>,
) -> Result<QualificationAssessment, ExactError> {
    qualification.validate()?;
    let mut gates = Vec::new();
    let mut blockers = Vec::new();
    let mut evidence_refs = Vec::new();

    for requirement in &qualification.requirements {
        let evaluation = evaluate_gate(qualification, requirement, as_of);
        if !evaluation.evidence.is_empty() {
            for reference in &evaluation.evidence {
                if !evidence_refs.contains(reference) {
                    evidence_refs.push(reference.clone());
                }
            }
        }
        if requirement.mandatory {
            match &evaluation.outcome {
                GateOutcome::Satisfied { .. } => {}
                GateOutcome::Pending { detail } => {
                    blockers.push(format!("gate {} pending: {detail}", requirement.id));
                }
                GateOutcome::Failed { detail } => {
                    blockers.push(format!("gate {} failed: {detail}", requirement.id));
                }
                GateOutcome::Conflicting { values } => {
                    blockers.push(format!(
                        "gate {} conflict: {}",
                        requirement.id,
                        values.join(" / ")
                    ));
                }
            }
        }
        gates.push(evaluation);
    }

    let approval = evaluate_approval(qualification, as_of, &mut blockers);
    for approval in &qualification.approvals {
        if !evidence_refs.contains(&approval.evidence) {
            evidence_refs.push(approval.evidence.clone());
        }
    }
    if let Some(suspension) = &qualification.suspension {
        if !evidence_refs.contains(&suspension.evidence) {
            evidence_refs.push(suspension.evidence.clone());
        }
    }
    let mut state = derive_state(qualification, &gates, &approval, as_of, &blockers);

    if let Some(suspension) = &qualification.suspension {
        let active = suspension.until.is_none_or(|until| !until.before(as_of));
        if active {
            state = QualificationState::Suspended;
            blockers.push(format!("suspension active: {}", suspension.reason));
        }
    }
    if let Some(valid_until) = qualification.valid_until {
        if valid_until.before(as_of)
            && !matches!(
                state,
                QualificationState::Suspended | QualificationState::Disqualified
            )
        {
            state = QualificationState::Expired;
            blockers.push(format!("qualification expired on day {}", valid_until.0));
        }
    }
    if matches!(
        state,
        QualificationState::Qualified | QualificationState::Conditional
    ) {
        let check = qualification_approval_check(qualification)?;
        let token =
            approval_token.ok_or_else(|| faktor_approval::ApprovalError::ApprovalRequired {
                kind: check.kind,
                subject: check.subject.clone(),
                bound_revision: check.bound_revision.clone(),
                bound_digest: check.bound_digest.clone(),
            })?;
        token.require(&check).map_err(ExactError::Approval)?;
    }
    Ok(QualificationAssessment {
        id: qualification.id.clone(),
        subject: qualification.subject.clone(),
        as_of,
        revision: qualification.revision,
        state,
        approval,
        gates,
        blockers,
        evidence: evidence_refs,
    })
}

fn derive_state(
    qualification: &Qualification,
    gates: &[GateEvaluation],
    approval: &ApprovalAssessment,
    as_of: Day,
    blockers: &[String],
) -> QualificationState {
    if qualification.requirements.is_empty() {
        return QualificationState::NotStarted;
    }
    if gates
        .iter()
        .any(|gate| gate.mandatory && matches!(gate.outcome, GateOutcome::Failed { .. }))
    {
        return QualificationState::Disqualified;
    }
    if gates
        .iter()
        .any(|gate| gate.mandatory && matches!(gate.outcome, GateOutcome::Conflicting { .. }))
    {
        return QualificationState::EvidencePending;
    }
    if gates
        .iter()
        .any(|gate| gate.mandatory && matches!(gate.outcome, GateOutcome::Pending { .. }))
    {
        return QualificationState::EvidencePending;
    }
    if let Some(valid_until) = approval_valid_until(approval, qualification) {
        if valid_until.before(as_of) {
            return QualificationState::Expired;
        }
    }
    match approval {
        ApprovalAssessment::Valid { conditions, .. } => {
            if conditions.is_empty() {
                QualificationState::Qualified
            } else {
                QualificationState::Conditional
            }
        }
        ApprovalAssessment::Absent
        | ApprovalAssessment::StaleRevision { .. }
        | ApprovalAssessment::Expired { .. }
        | ApprovalAssessment::FutureDated { .. }
        | ApprovalAssessment::Conflicting { .. } => {
            let _ = blockers;
            QualificationState::EvidencePending
        }
    }
}

fn approval_valid_until(
    approval: &ApprovalAssessment,
    qualification: &Qualification,
) -> Option<Day> {
    match approval {
        ApprovalAssessment::Valid { .. } => qualification
            .approvals
            .iter()
            .find(|approval| approval.approved_revision == qualification.revision)
            .and_then(|approval| approval.valid_until),
        _ => None,
    }
}

fn evaluate_approval(
    qualification: &Qualification,
    as_of: Day,
    blockers: &mut Vec<String>,
) -> ApprovalAssessment {
    let mut valid = Vec::new();
    let mut stale = Vec::new();
    let mut expired = Vec::new();
    let mut future = Vec::new();
    for approval in &qualification.approvals {
        if approval.approved_revision != qualification.revision {
            stale.push(approval.clone());
        } else if approval.approved_on.0 > as_of.0 {
            future.push(approval.clone());
        } else if approval
            .valid_until
            .is_some_and(|valid_until| valid_until.before(as_of))
        {
            expired.push(approval.clone());
        } else {
            valid.push(approval.clone());
        }
    }
    if valid.len() > 1 {
        let mut descriptions = Vec::new();
        for approval in &valid {
            descriptions.push(format!(
                "{} approved {} with conditions [{}]",
                approval.authority,
                approval.approved_on.0,
                approval.conditions.join("; ")
            ));
        }
        descriptions.sort();
        descriptions.dedup();
        if descriptions.len() > 1 {
            blockers.push("conflicting approvals".to_string());
            return ApprovalAssessment::Conflicting {
                values: descriptions,
            };
        }
    }
    if let Some(approval) = valid.into_iter().next() {
        return ApprovalAssessment::Valid {
            authority: approval.authority,
            conditions: approval.conditions,
        };
    }
    if let Some(approval) = future.into_iter().next() {
        blockers.push(format!(
            "approval dated {} is after the assessment day {}",
            approval.approved_on.0, as_of.0
        ));
        return ApprovalAssessment::FutureDated {
            approved_on: approval.approved_on,
        };
    }
    if let Some(approval) = stale.into_iter().next() {
        blockers.push(format!(
            "approval is for revision {} but the record is revision {}",
            approval.approved_revision.get(),
            qualification.revision.get()
        ));
        return ApprovalAssessment::StaleRevision {
            approved_revision: approval.approved_revision,
            current_revision: qualification.revision,
        };
    }
    if let Some(approval) = expired.into_iter().next() {
        let valid_until = approval.valid_until.unwrap_or(as_of);
        blockers.push(format!("approval expired on day {}", valid_until.0));
        return ApprovalAssessment::Expired { valid_until };
    }
    blockers.push("no approval is bound to the current revision".to_string());
    ApprovalAssessment::Absent
}

fn evaluate_gate(
    qualification: &Qualification,
    requirement: &GateRequirement,
    as_of: Day,
) -> GateEvaluation {
    if matches!(requirement.spec, GateSpec::NcrClosure { .. }) {
        return evaluate_ncr_gate(qualification, requirement);
    }
    let expected_kind = requirement.spec.label();
    let mut candidates = Vec::new();
    for record in &qualification.evidence {
        if record.kind_label() != expected_kind {
            continue;
        }
        if record.bound_revision != qualification.revision {
            continue;
        }
        if let Err(outcome) = evidence_current(record, requirement, as_of) {
            candidates.push((record, Err(outcome)));
        } else {
            candidates.push((record, Ok(())));
        }
    }
    if candidates.is_empty() {
        return GateEvaluation {
            id: requirement.id.clone(),
            gate: expected_kind.to_string(),
            mandatory: requirement.mandatory,
            outcome: GateOutcome::Pending {
                detail: format!("no {expected_kind} evidence for the current revision"),
            },
            evidence: Vec::new(),
        };
    }
    let mut satisfied = Vec::new();
    let mut pending = Vec::new();
    let mut failed = Vec::new();
    let mut evidence = Vec::new();
    for (record, current) in candidates {
        evidence.push(record.evidence.clone());
        if let Err(outcome) = current {
            pending.push(outcome);
            continue;
        }
        match evaluate_payload(&requirement.spec, &record.payload) {
            GateOutcome::Satisfied { detail } => satisfied.push(detail),
            GateOutcome::Pending { detail } => pending.push(detail),
            GateOutcome::Failed { detail } => failed.push(detail),
            GateOutcome::Conflicting { values } => failed.extend(values),
        }
    }
    dedup_strings(&mut satisfied);
    dedup_strings(&mut pending);
    dedup_strings(&mut failed);
    dedup_evidence(&mut evidence);
    let outcome = if !satisfied.is_empty() && !failed.is_empty() {
        let mut values = satisfied;
        values.extend(failed);
        values.sort();
        values.dedup();
        GateOutcome::Conflicting { values }
    } else if !failed.is_empty() {
        GateOutcome::Failed {
            detail: failed.join("; "),
        }
    } else if !satisfied.is_empty() {
        GateOutcome::Satisfied {
            detail: satisfied.join("; "),
        }
    } else {
        GateOutcome::Pending {
            detail: pending
                .first()
                .cloned()
                .unwrap_or_else(|| "evidence pending".to_string()),
        }
    };
    GateEvaluation {
        id: requirement.id.clone(),
        gate: expected_kind.to_string(),
        mandatory: requirement.mandatory,
        outcome,
        evidence,
    }
}

fn evidence_current(
    record: &GateEvidence,
    requirement: &GateRequirement,
    as_of: Day,
) -> Result<(), String> {
    if record.captured_on.0 > as_of.0 {
        return Err(format!(
            "evidence captured on day {} is after the assessment day {}",
            record.captured_on.0, as_of.0
        ));
    }
    if let Some(max_age) = requirement.max_age_days {
        let age = as_of.0 - record.captured_on.0;
        if age > i64::from(max_age) {
            return Err(format!(
                "evidence age {age} days exceeds the {max_age}-day limit"
            ));
        }
    }
    if let Some(valid_until) = record.valid_until {
        if valid_until.before(as_of) {
            return Err(format!("evidence expired on day {}", valid_until.0));
        }
    }
    Ok(())
}

fn evaluate_payload(spec: &GateSpec, payload: &EvidencePayload) -> GateOutcome {
    match (spec, payload) {
        (
            GateSpec::Ppap {
                min_level,
                require_psw,
                required_elements,
            },
            EvidencePayload::Ppap(submission),
        ) => {
            if submission.level.number() < min_level.number() {
                return GateOutcome::Pending {
                    detail: format!(
                        "PPAP level {} is below required level {}",
                        submission.level.number(),
                        min_level.number()
                    ),
                };
            }
            if *require_psw && !submission.psw_signed {
                return GateOutcome::Pending {
                    detail: "PSW is not signed".to_string(),
                };
            }
            let mut required = if required_elements.is_empty() {
                if submission.level == PpapLevel::Level4 {
                    return GateOutcome::Pending {
                        detail: "level 4 PPAP requires a customer-defined element list".to_string(),
                    };
                }
                submission.level.modeled_elements()
            } else {
                required_elements.clone()
            };
            required.sort();
            required.dedup();
            let missing = required
                .into_iter()
                .filter(|element| !submission.elements.contains(element))
                .collect::<Vec<_>>();
            if missing.is_empty() {
                GateOutcome::Satisfied {
                    detail: format!("PPAP level {} complete", submission.level.number()),
                }
            } else {
                GateOutcome::Pending {
                    detail: format!("PPAP missing elements: {missing:?}"),
                }
            }
        }
        (GateSpec::Apqp { required_phase }, EvidencePayload::Apqp(submission)) => {
            if submission.completed_phases.contains(required_phase) {
                GateOutcome::Satisfied {
                    detail: format!("APQP phase {required_phase:?} complete"),
                }
            } else {
                GateOutcome::Pending {
                    detail: format!("APQP phase {required_phase:?} not complete"),
                }
            }
        }
        (GateSpec::Fai, EvidencePayload::Fai(report)) => {
            if report.open_nonconformances > 0 {
                GateOutcome::Failed {
                    detail: format!(
                        "FAI has {} open nonconformances",
                        report.open_nonconformances
                    ),
                }
            } else if !report.ballooned_drawing {
                GateOutcome::Pending {
                    detail: "FAI drawing is not ballooned".to_string(),
                }
            } else if report.characteristics_verified < report.characteristics_total {
                GateOutcome::Pending {
                    detail: format!(
                        "FAI verified {}/{} characteristics",
                        report.characteristics_verified, report.characteristics_total
                    ),
                }
            } else {
                GateOutcome::Satisfied {
                    detail: format!(
                        "FAI verified {} characteristics with no open nonconformance",
                        report.characteristics_total
                    ),
                }
            }
        }
        (
            GateSpec::Audit {
                max_open_findings,
                max_critical_findings,
            },
            EvidencePayload::Audit(report),
        ) => {
            if report.critical_findings > *max_critical_findings {
                GateOutcome::Failed {
                    detail: format!(
                        "{} critical audit findings exceed {}",
                        report.critical_findings, max_critical_findings
                    ),
                }
            } else if report.open_findings > *max_open_findings {
                GateOutcome::Failed {
                    detail: format!(
                        "{} open audit findings exceed {}",
                        report.open_findings, max_open_findings
                    ),
                }
            } else {
                GateOutcome::Satisfied {
                    detail: format!(
                        "{} open findings, {} critical",
                        report.open_findings, report.critical_findings
                    ),
                }
            }
        }
        (
            GateSpec::Msa {
                max_grr_ppm,
                min_sample,
            },
            EvidencePayload::Msa(report),
        ) => {
            if report.sample_size < *min_sample {
                GateOutcome::Pending {
                    detail: format!("MSA sample {} is below {}", report.sample_size, min_sample),
                }
            } else if report.grr_ppm > *max_grr_ppm {
                GateOutcome::Failed {
                    detail: format!("gauge R&R {} ppm exceeds {}", report.grr_ppm, max_grr_ppm),
                }
            } else if !report.accepted_by_authority {
                GateOutcome::Pending {
                    detail: "MSA not accepted by the quality authority".to_string(),
                }
            } else {
                GateOutcome::Satisfied {
                    detail: format!("gauge R&R {} ppm", report.grr_ppm),
                }
            }
        }
        (
            GateSpec::ProcessCapability {
                min_cpk,
                min_ppk,
                min_sample,
            },
            EvidencePayload::ProcessCapability(submission),
        ) => {
            if submission.sample_size < *min_sample {
                GateOutcome::Pending {
                    detail: format!(
                        "capability sample {} is below {}",
                        submission.sample_size, min_sample
                    ),
                }
            } else if submission.cpk < *min_cpk {
                GateOutcome::Failed {
                    detail: format!("Cpk {} below {}", submission.cpk, min_cpk),
                }
            } else if submission.ppk < *min_ppk {
                GateOutcome::Failed {
                    detail: format!("Ppk {} below {}", submission.ppk, min_ppk),
                }
            } else {
                GateOutcome::Satisfied {
                    detail: format!("Cpk {} Ppk {}", submission.cpk, submission.ppk),
                }
            }
        }
        (GateSpec::Traceability { min_depth }, EvidencePayload::Traceability(submission)) => {
            if submission.depth < *min_depth {
                GateOutcome::Failed {
                    detail: format!(
                        "traceability depth {:?} is below {:?}",
                        submission.depth, min_depth
                    ),
                }
            } else if !submission.records_available {
                GateOutcome::Pending {
                    detail: "production records are not retrievable".to_string(),
                }
            } else {
                GateOutcome::Satisfied {
                    detail: format!("traceability depth {:?}", submission.depth),
                }
            }
        }
        (GateSpec::PcnReview { pcn_id }, EvidencePayload::PcnReview(review)) => {
            if &review.pcn_id != pcn_id {
                GateOutcome::Pending {
                    detail: format!("PCN evidence is for {}, not {pcn_id}", review.pcn_id),
                }
            } else if !review.acknowledged {
                GateOutcome::Pending {
                    detail: "PCN not acknowledged".to_string(),
                }
            } else if !review.impact_assessed {
                GateOutcome::Pending {
                    detail: "PCN impact not assessed".to_string(),
                }
            } else {
                GateOutcome::Satisfied {
                    detail: format!("PCN {pcn_id} reviewed"),
                }
            }
        }
        _ => GateOutcome::Pending {
            detail: "evidence type does not match the gate".to_string(),
        },
    }
}

fn evaluate_ncr_gate(
    qualification: &Qualification,
    requirement: &GateRequirement,
) -> GateEvaluation {
    let GateSpec::NcrClosure { ncr_ids } = &requirement.spec else {
        unreachable!("only called for NCR closure gates");
    };
    let mut evidence = Vec::new();
    let mut pending = Vec::new();
    let mut failed = Vec::new();
    for id in ncr_ids {
        match qualification.ncrs.iter().find(|ncr| &ncr.id == id) {
            None => pending.push(format!("NCR {id} not found")),
            Some(ncr) => {
                if let Some(reference) = &ncr.containment_evidence {
                    evidence.push(reference.clone());
                }
                if let Some(reference) = &ncr.corrective_action_evidence {
                    evidence.push(reference.clone());
                }
                match ncr.status {
                    NcrStatus::Closed | NcrStatus::Void => {}
                    NcrStatus::Open => pending.push(format!("NCR {id} is open")),
                    NcrStatus::Contained => {
                        pending.push(format!("NCR {id} has containment but no closure"))
                    }
                    NcrStatus::CorrectiveActionPending => {
                        pending.push(format!("NCR {id} corrective action is pending"))
                    }
                }
                if ncr.severity == NcrSeverity::Critical
                    && matches!(ncr.status, NcrStatus::Open | NcrStatus::Contained)
                {
                    failed.push(format!("critical NCR {id} is unresolved"));
                }
            }
        }
    }
    let outcome = if !failed.is_empty() {
        GateOutcome::Failed {
            detail: failed.join("; "),
        }
    } else if !pending.is_empty() {
        GateOutcome::Pending {
            detail: pending.join("; "),
        }
    } else {
        GateOutcome::Satisfied {
            detail: format!("{} NCRs closed", ncr_ids.len()),
        }
    };
    dedup_evidence(&mut evidence);
    GateEvaluation {
        id: requirement.id.clone(),
        gate: "ncr_closure".to_string(),
        mandatory: requirement.mandatory,
        outcome,
        evidence,
    }
}

fn dedup_strings(values: &mut Vec<String>) {
    values.sort();
    values.dedup();
}

fn dedup_evidence(evidence: &mut Vec<EvidenceRef>) {
    let mut seen = HashSet::new();
    evidence.retain(|reference| seen.insert(reference.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human_assess(
        qualification: &Qualification,
        as_of: Day,
    ) -> Result<QualificationAssessment, ExactError> {
        let check = qualification_approval_check(qualification)?;
        let token = faktor_approval::ApprovalToken::test_token(
            check.kind,
            check.subject,
            check.bound_revision,
            check.bound_digest,
        );
        assess_with_approval(qualification, as_of, Some(&token))
    }
    use faktor_supply_core::{Provenance, SourceKind};

    const DAY: i64 = 20_000;

    fn evidence(id: &str, revision: u64) -> EvidenceRef {
        EvidenceRef::new(id, Revision(revision), Some("digest")).unwrap()
    }

    fn provenance(id: &str) -> Provenance {
        Provenance::new(
            SourceKind::TestReport,
            evidence(id, 1),
            Day(DAY - 5),
            "fixture",
            vec!["sample period".to_string()],
            "a failed verification",
        )
        .unwrap()
    }

    fn record(id: &str, payload: EvidencePayload, captured_on: Day) -> GateEvidence {
        GateEvidence {
            id: id.to_string(),
            evidence: provenance(id).evidence,
            bound_revision: Revision(7),
            captured_on,
            valid_until: None,
            payload,
        }
    }

    fn full_qualification() -> Qualification {
        let ppap_elements = PpapLevel::Level3.modeled_elements();
        Qualification {
            id: "q-1".to_string(),
            subject: "supplier 1 / part 9".to_string(),
            revision: Revision(7),
            valid_until: Some(Day(DAY + 365)),
            requirements: vec![
                GateRequirement {
                    id: "g-ppap".to_string(),
                    spec: GateSpec::Ppap {
                        min_level: PpapLevel::Level3,
                        require_psw: true,
                        required_elements: vec![],
                    },
                    mandatory: true,
                    max_age_days: Some(400),
                },
                GateRequirement {
                    id: "g-fai".to_string(),
                    spec: GateSpec::Fai,
                    mandatory: true,
                    max_age_days: Some(400),
                },
                GateRequirement {
                    id: "g-ncr".to_string(),
                    spec: GateSpec::NcrClosure {
                        ncr_ids: vec!["ncr-1".to_string()],
                    },
                    mandatory: true,
                    max_age_days: None,
                },
                GateRequirement {
                    id: "g-cpk".to_string(),
                    spec: GateSpec::ProcessCapability {
                        min_cpk: 1_330_000,
                        min_ppk: 1_330_000,
                        min_sample: 30,
                    },
                    mandatory: true,
                    max_age_days: Some(400),
                },
                GateRequirement {
                    id: "g-pcn".to_string(),
                    spec: GateSpec::PcnReview {
                        pcn_id: "pcn-1".to_string(),
                    },
                    mandatory: false,
                    max_age_days: None,
                },
            ],
            evidence: vec![
                record(
                    "ppap-ev",
                    EvidencePayload::Ppap(PpapSubmission {
                        level: PpapLevel::Level3,
                        psw_signed: true,
                        elements: ppap_elements,
                        submitted_on: Day(DAY - 3),
                    }),
                    Day(DAY - 3),
                ),
                record(
                    "fai-ev",
                    EvidencePayload::Fai(FaiReport {
                        characteristics_total: 42,
                        characteristics_verified: 42,
                        open_nonconformances: 0,
                        ballooned_drawing: true,
                        performed_on: Day(DAY - 4),
                    }),
                    Day(DAY - 4),
                ),
                record(
                    "cpk-ev",
                    EvidencePayload::ProcessCapability(CapabilitySubmission {
                        cpk: 1_670_000,
                        ppk: 1_500_000,
                        sample_size: 125,
                        measured_on: Day(DAY - 10),
                    }),
                    Day(DAY - 10),
                ),
                record(
                    "pcn-ev",
                    EvidencePayload::PcnReview(PcnReview {
                        pcn_id: "pcn-1".to_string(),
                        acknowledged: true,
                        impact_assessed: true,
                    }),
                    Day(DAY - 8),
                ),
            ],
            ncrs: vec![NcrRecord {
                id: "ncr-1".to_string(),
                severity: NcrSeverity::Major,
                status: NcrStatus::Closed,
                disposition: NcrDisposition::Rework,
                containment_evidence: Some(evidence("contain", 1)),
                corrective_action_evidence: Some(evidence("correct", 1)),
                opened_on: Day(DAY - 30),
                closed_on: Some(Day(DAY - 20)),
                void_reason: None,
            }],
            approvals: vec![Approval {
                authority: "quality-director".to_string(),
                evidence: evidence("approval-sig", 1),
                approved_revision: Revision(7),
                approved_on: Day(DAY - 2),
                valid_until: Some(Day(DAY + 365)),
                conditions: vec![],
            }],
            suspension: None,
        }
    }

    #[test]
    fn complete_current_evidence_qualifies() {
        let assessment = human_assess(&full_qualification(), Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Qualified);
        assert!(assessment.blockers.is_empty());
        assert!(matches!(
            assessment.approval,
            ApprovalAssessment::Valid { .. }
        ));
    }

    #[test]
    fn a_qualified_result_without_a_human_approval_token_is_refused() {
        let error = assess(&full_qualification(), Day(DAY)).unwrap_err();
        assert!(matches!(
            error,
            ExactError::Approval(faktor_approval::ApprovalError::ApprovalRequired { .. })
        ));
    }

    #[test]
    fn evidence_alone_is_not_approval() {
        let mut qualification = full_qualification();
        qualification.approvals.clear();
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
        assert!(matches!(assessment.approval, ApprovalAssessment::Absent));
    }

    #[test]
    fn approval_of_an_old_revision_cannot_approve_the_current_one() {
        let mut qualification = full_qualification();
        qualification.revision = Revision(8);
        // Evidence is also bound to the old revision, so the state is pending.
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
        assert!(matches!(
            assessment.approval,
            ApprovalAssessment::StaleRevision { .. }
        ));
    }

    #[test]
    fn approval_with_conditions_is_conditional() {
        let mut qualification = full_qualification();
        qualification.approvals[0].conditions = vec!["100% inspection for 3 lots".to_string()];
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Conditional);
    }

    #[test]
    fn failed_fai_disqualifies() {
        let mut qualification = full_qualification();
        qualification.evidence[1] = record(
            "fai-ev",
            EvidencePayload::Fai(FaiReport {
                characteristics_total: 42,
                characteristics_verified: 42,
                open_nonconformances: 2,
                ballooned_drawing: true,
                performed_on: Day(DAY - 4),
            }),
            Day(DAY - 4),
        );
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Disqualified);
        assert!(assessment
            .blockers
            .iter()
            .any(|blocker| blocker.contains("failed")));
    }

    #[test]
    fn open_ncr_blocks_qualification() {
        let mut qualification = full_qualification();
        qualification.ncrs[0].status = NcrStatus::Contained;
        qualification.ncrs[0].closed_on = None;
        qualification.ncrs[0].corrective_action_evidence = None;
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
        assert!(assessment
            .gates
            .iter()
            .any(|gate| gate.gate == "ncr_closure"
                && matches!(gate.outcome, GateOutcome::Pending { .. })));
    }

    #[test]
    fn closing_an_ncr_without_evidence_is_refused_at_validation() {
        let mut qualification = full_qualification();
        qualification.ncrs[0].corrective_action_evidence = None;
        // Breaks if an NCR can be closed by assertion rather than evidence.
        assert!(matches!(
            qualification.validate(),
            Err(ExactError::OutOfBounds {
                what: "NCR",
                detail,
            }) if detail.contains("corrective-action evidence")
        ));
    }

    #[test]
    fn incomplete_ppap_is_pending_not_approved() {
        let mut qualification = full_qualification();
        if let EvidencePayload::Ppap(ppap) = &mut qualification.evidence[0].payload {
            ppap.elements
                .retain(|element| *element != PpapElement::ControlPlan);
        }
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
        assert!(assessment
            .blockers
            .iter()
            .any(|blocker| blocker.contains("PPAP")));
    }

    #[test]
    fn level_four_without_customer_elements_is_pending() {
        let mut qualification = full_qualification();
        qualification.requirements[0].spec = GateSpec::Ppap {
            min_level: PpapLevel::Level4,
            require_psw: true,
            required_elements: vec![],
        };
        if let EvidencePayload::Ppap(ppap) = &mut qualification.evidence[0].payload {
            ppap.level = PpapLevel::Level4;
        }
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
    }

    #[test]
    fn unsigned_psw_is_pending() {
        let mut qualification = full_qualification();
        if let EvidencePayload::Ppap(ppap) = &mut qualification.evidence[0].payload {
            ppap.psw_signed = false;
        }
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
    }

    #[test]
    fn expired_evidence_blocks() {
        let mut qualification = full_qualification();
        qualification.evidence[0].valid_until = Some(Day(DAY - 1));
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
    }

    #[test]
    fn evidence_older_than_gate_limit_blocks() {
        let mut qualification = full_qualification();
        qualification.requirements[0].max_age_days = Some(1);
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
        assert!(assessment
            .blockers
            .iter()
            .any(|blocker| blocker.contains("age")));
    }

    #[test]
    fn capability_below_gate_disqualifies() {
        let mut qualification = full_qualification();
        if let EvidencePayload::ProcessCapability(capability) =
            &mut qualification.evidence[2].payload
        {
            capability.cpk = 1_100_000;
        }
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Disqualified);
    }

    #[test]
    fn traceability_and_msa_and_audit_gates() {
        let mut qualification = full_qualification();
        qualification.requirements.push(GateRequirement {
            id: "g-trace".to_string(),
            spec: GateSpec::Traceability {
                min_depth: TraceabilityDepth::Serial,
            },
            mandatory: true,
            max_age_days: None,
        });
        qualification.requirements.push(GateRequirement {
            id: "g-audit".to_string(),
            spec: GateSpec::Audit {
                max_open_findings: 2,
                max_critical_findings: 0,
            },
            mandatory: true,
            max_age_days: None,
        });
        qualification.requirements.push(GateRequirement {
            id: "g-msa".to_string(),
            spec: GateSpec::Msa {
                max_grr_ppm: 100_000,
                min_sample: 30,
            },
            mandatory: true,
            max_age_days: None,
        });
        qualification.evidence.push(record(
            "trace-ev",
            EvidencePayload::Traceability(TraceabilitySubmission {
                depth: TraceabilityDepth::Serial,
                records_available: true,
            }),
            Day(DAY - 1),
        ));
        qualification.evidence.push(record(
            "audit-ev",
            EvidencePayload::Audit(AuditReport {
                open_findings: 1,
                critical_findings: 0,
                audited_on: Day(DAY - 1),
            }),
            Day(DAY - 1),
        ));
        qualification.evidence.push(record(
            "msa-ev",
            EvidencePayload::Msa(MsaReport {
                grr_ppm: 50_000,
                sample_size: 50,
                accepted_by_authority: true,
            }),
            Day(DAY - 1),
        ));
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Qualified);
    }

    #[test]
    fn active_suspension_wins() {
        let mut qualification = full_qualification();
        qualification.suspension = Some(Suspension {
            reason: "containment failure".to_string(),
            evidence: evidence("suspension", 1),
            until: None,
        });
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Suspended);
    }

    #[test]
    fn expired_qualification_is_expired() {
        let mut qualification = full_qualification();
        qualification.valid_until = Some(Day(DAY - 1));
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Expired);
    }

    #[test]
    fn future_dated_approval_cannot_approve_today() {
        let mut qualification = full_qualification();
        qualification.approvals[0].approved_on = Day(DAY + 1);
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
        assert!(matches!(
            assessment.approval,
            ApprovalAssessment::FutureDated { .. }
        ));
    }

    #[test]
    fn conflicting_approvals_are_surfaced() {
        let mut qualification = full_qualification();
        let mut second = qualification.approvals[0].clone();
        second.authority = "plant-quality".to_string();
        second.evidence = evidence("approval-sig-2", 1);
        second.conditions = vec!["stop shipment on any NCR".to_string()];
        qualification.approvals.push(second);
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
        assert!(matches!(
            assessment.approval,
            ApprovalAssessment::Conflicting { .. }
        ));
    }

    #[test]
    fn lifecycle_transitions_are_explicit() {
        assert!(can_transition(
            QualificationState::EvidencePending,
            QualificationState::Qualified
        ));
        assert!(can_transition(
            QualificationState::Qualified,
            QualificationState::Suspended
        ));
        assert!(!can_transition(
            QualificationState::NotStarted,
            QualificationState::Qualified
        ));
        assert!(!can_transition(
            QualificationState::Suspended,
            QualificationState::Qualified
        ));
    }

    #[test]
    fn empty_qualification_is_not_started() {
        let qualification = Qualification {
            id: "q-empty".to_string(),
            subject: "empty".to_string(),
            revision: Revision(1),
            valid_until: None,
            requirements: vec![],
            evidence: vec![],
            ncrs: vec![],
            approvals: vec![],
            suspension: None,
        };
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::NotStarted);
    }

    #[test]
    fn duplicate_gate_ids_and_stale_approval_are_refused() {
        let mut qualification = full_qualification();
        let gate = qualification.requirements[0].clone();
        qualification.requirements.push(gate);
        // Breaks if duplicate gate ids are accepted or reported untyped.
        assert!(matches!(
            qualification.validate(),
            Err(ExactError::OutOfBounds {
                what: "gate ids",
                ..
            })
        ));

        let mut approval = full_qualification();
        approval.approvals[0].approved_revision = Revision(99);
        let assessment = human_assess(&approval, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::EvidencePending);
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct QualificationFixture {
        qualification: Qualification,
        as_of: Day,
    }

    #[test]
    fn json_fixture_qualifies_with_evidence_and_approval() {
        let fixture: QualificationFixture =
            serde_json::from_str(include_str!("../tests/fixtures/qualification.json")).unwrap();
        let assessment = human_assess(&fixture.qualification, fixture.as_of).unwrap();
        assert_eq!(assessment.state, QualificationState::Qualified);
        assert!(assessment.blockers.is_empty());
    }

    #[test]
    fn pcn_unacknowledged_is_pending_but_optional_does_not_block() {
        let mut qualification = full_qualification();
        if let EvidencePayload::PcnReview(pcn) = &mut qualification.evidence[3].payload {
            pcn.acknowledged = false;
        }
        let assessment = human_assess(&qualification, Day(DAY)).unwrap();
        assert_eq!(assessment.state, QualificationState::Qualified);
        assert!(assessment
            .gates
            .iter()
            .any(|gate| gate.gate == "pcn_review"
                && matches!(gate.outcome, GateOutcome::Pending { .. })));
    }
}
