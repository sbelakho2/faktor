//! Evidence, revisions, assumptions and the typed unknown.
//!
//! Three laws are structural here:
//!
//! 1. A known value in a [`Term`] must carry either evidence
//!    ([`Basis::Provenance`]) or an explicit [`Assumption`]. There is no
//!    constructor that silently produces an unbound known value.
//! 2. Unknown is first-class: [`Availability::Unavailable`] carries a typed
//!    [`UnavailableReason`]. It is never coerced to zero.
//! 3. Evidence is revision-bound and condition-bound. Every assertion says
//!    under which conditions it holds and how it could be disproved.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ExactError;

/// Maximum bytes in a short identifier or name.
pub const MAX_ID_BYTES: usize = 128;
/// Maximum bytes in a free-text statement, rationale or note.
pub const MAX_TEXT_BYTES: usize = 1_024;
/// Maximum entries in one provenance condition list.
pub const MAX_CONDITIONS: usize = 16;
/// Maximum entries in a term's assumption list.
pub const MAX_ASSUMPTIONS: usize = 16;

/// Validate bounded text without accepting empty identifiers.
pub fn validate_text(value: &str, max: usize, what: &'static str) -> Result<(), ExactError> {
    if value.is_empty() {
        return Err(ExactError::OutOfBounds {
            what,
            detail: "empty".to_string(),
        });
    }
    if value.len() > max {
        return Err(ExactError::OutOfBounds {
            what,
            detail: format!("{} bytes exceeds {max}", value.len()),
        });
    }
    Ok(())
}

// ------------------------------------------------------------------ revision

/// A monotonic revision number. Decisions and evidence are revision-bound.
#[derive(
    JsonSchema,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Default,
)]
#[serde(transparent)]
#[schemars(inline)]
pub struct Revision(pub u64);

impl Revision {
    /// Revision zero (the pre-evidence state).
    pub const INITIAL: Revision = Revision(0);

    /// The next revision, refusing overflow.
    pub fn next(self) -> Result<Self, ExactError> {
        self.0
            .checked_add(1)
            .map(Revision)
            .ok_or(ExactError::Overflow)
    }

    /// The raw value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A day number (days since 1970-01-01). No timezone, no wall clock: all
/// planning and expiry reasoning is deterministic over these integers.
#[derive(
    JsonSchema,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Default,
)]
#[serde(transparent)]
pub struct Day(pub i64);

impl Day {
    /// Add days.
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, days: i64) -> Result<Self, ExactError> {
        self.0
            .checked_add(days)
            .map(Day)
            .ok_or(ExactError::Overflow)
    }

    /// True when this day is before `other`.
    pub const fn before(self, other: Day) -> bool {
        self.0 < other.0
    }
}

// -------------------------------------------------------------------- evidence

/// The class of source a statement comes from.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A supplier-provided document (quote, datasheet, declaration).
    SupplierDocument,
    /// A test, inspection or metrology report.
    TestReport,
    /// An ERP / purchasing-system extract.
    ErpExtract,
    /// A purchasing record or order history.
    PurchasingRecord,
    /// An authoritative tariff schedule or customs ruling.
    PublicTariff,
    /// An engineering requirement or drawing.
    EngineeringRequirement,
    /// An audit report.
    AuditReport,
    /// An internal estimate, clearly marked as such.
    InternalEstimate,
    /// A market quote observed through the checked acquisition plane.
    MarketQuote,
    /// A deterministic calculator output (revision-bound kernel result).
    Calculator,
}

/// A revision-bound reference to a piece of evidence.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    /// Stable evidence identifier.
    pub id: String,
    /// The revision of the evidence at capture time.
    pub revision: Revision,
    /// Optional content digest (lowercase hex or another stable digest).
    pub digest: Option<String>,
}

impl EvidenceRef {
    /// Construct and validate a reference.
    pub fn new(id: &str, revision: Revision, digest: Option<&str>) -> Result<Self, ExactError> {
        validate_text(id, MAX_ID_BYTES, "evidence id")?;
        if let Some(digest) = digest {
            validate_text(digest, MAX_ID_BYTES, "evidence digest")?;
        }
        Ok(Self {
            id: id.to_string(),
            revision,
            digest: digest.map(str::to_string),
        })
    }

    /// Validate a deserialized reference.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.id, MAX_ID_BYTES, "evidence id")?;
        if let Some(digest) = &self.digest {
            validate_text(digest, MAX_ID_BYTES, "evidence digest")?;
        }
        Ok(())
    }
}

/// Why a statement is true, under which conditions, and how to disprove it.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Source class.
    pub source: SourceKind,
    /// The exact evidence revision.
    pub evidence: EvidenceRef,
    /// When the evidence was captured (day number).
    pub captured_on: Day,
    /// How the value was obtained (extraction/calculation method).
    pub method: String,
    /// Conditions under which the statement holds.
    #[serde(default)]
    pub conditions: Vec<String>,
    /// What observation would disprove the statement.
    pub falsifier: String,
}

impl Provenance {
    /// Construct and validate provenance.
    pub fn new(
        source: SourceKind,
        evidence: EvidenceRef,
        captured_on: Day,
        method: &str,
        conditions: Vec<String>,
        falsifier: &str,
    ) -> Result<Self, ExactError> {
        if conditions.len() > MAX_CONDITIONS {
            return Err(ExactError::OutOfBounds {
                what: "conditions",
                detail: format!("{} entries exceeds {MAX_CONDITIONS}", conditions.len()),
            });
        }
        let provenance = Self {
            source,
            evidence,
            captured_on,
            method: method.to_string(),
            conditions,
            falsifier: falsifier.to_string(),
        };
        provenance.validate()?;
        Ok(provenance)
    }

    /// Validate a deserialized provenance record.
    pub fn validate(&self) -> Result<(), ExactError> {
        self.evidence.validate()?;
        validate_text(&self.method, MAX_TEXT_BYTES, "provenance method")?;
        validate_text(&self.falsifier, MAX_TEXT_BYTES, "provenance falsifier")?;
        if self.conditions.len() > MAX_CONDITIONS {
            return Err(ExactError::OutOfBounds {
                what: "conditions",
                detail: format!("{} entries", self.conditions.len()),
            });
        }
        for condition in &self.conditions {
            validate_text(condition, MAX_TEXT_BYTES, "provenance condition")?;
        }
        Ok(())
    }
}

/// An explicit typed assumption with provenance and a falsifier.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assumption {
    /// Stable assumption identifier (unique within a decision).
    pub id: String,
    /// The assumed statement.
    pub statement: String,
    /// Why the assumption is made.
    pub rationale: String,
    /// Where the assumption came from (or the internal estimate basis).
    pub provenance: Provenance,
    /// What would disprove it.
    pub falsifier: String,
}

impl Assumption {
    /// Construct and validate an assumption.
    pub fn new(
        id: &str,
        statement: &str,
        rationale: &str,
        provenance: Provenance,
        falsifier: &str,
    ) -> Result<Self, ExactError> {
        let assumption = Self {
            id: id.to_string(),
            statement: statement.to_string(),
            rationale: rationale.to_string(),
            provenance,
            falsifier: falsifier.to_string(),
        };
        assumption.validate()?;
        Ok(assumption)
    }

    /// Validate a deserialized assumption.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.id, MAX_ID_BYTES, "assumption id")?;
        validate_text(&self.statement, MAX_TEXT_BYTES, "assumption statement")?;
        validate_text(&self.rationale, MAX_TEXT_BYTES, "assumption rationale")?;
        validate_text(&self.falsifier, MAX_TEXT_BYTES, "assumption falsifier")?;
        self.provenance.validate()
    }
}

/// The basis of one term: evidence, an explicit assumption, or none (legal
/// only when the term is unknown/not applicable).
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Basis {
    /// Backed by captured evidence.
    Provenance(Provenance),
    /// Explicitly assumed, with its own provenance and falsifier.
    Assumption(Assumption),
}

// ------------------------------------------------------------------ availability

/// Why a value is unavailable. The reason is typed; zero is never a stand-in.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// The required evidence was not supplied.
    MissingEvidence,
    /// The party was asked and did not provide the value.
    NotProvided,
    /// Supplied facts support several incompatible values/classifications.
    Ambiguous,
    /// The evidence exists but is outside its validity period.
    Expired,
    /// Two authorities conflict; the conflict is surfaced, never averaged.
    ConflictingEvidence,
    /// The term does not apply to this case (explicitly declared).
    NotApplicable,
    /// A supplied value is outside its permitted domain.
    OutOfRange,
    /// Exact arithmetic would overflow or divide by zero.
    Arithmetic,
    /// No candidate satisfies the requirement.
    NoCandidate,
}

impl UnavailableReason {
    /// Stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingEvidence => "missing_evidence",
            Self::NotProvided => "not_provided",
            Self::Ambiguous => "ambiguous",
            Self::Expired => "expired",
            Self::ConflictingEvidence => "conflicting_evidence",
            Self::NotApplicable => "not_applicable",
            Self::OutOfRange => "out_of_range",
            Self::Arithmetic => "arithmetic",
            Self::NoCandidate => "no_candidate",
        }
    }
}

/// A typed unavailable value with a human-readable, bounded detail.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unavailable {
    /// The typed reason.
    pub reason: UnavailableReason,
    /// Optional bounded detail (never the sole carrier of the reason).
    #[serde(default)]
    pub detail: Option<String>,
}

impl Unavailable {
    /// Construct an unavailable value.
    pub fn new(reason: UnavailableReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: Some(detail.into()),
        }
    }

    /// Construct without extra detail.
    pub const fn bare(reason: UnavailableReason) -> Self {
        Self {
            reason,
            detail: None,
        }
    }

    /// True for explicitly not applicable.
    pub const fn is_not_applicable(&self) -> bool {
        matches!(self.reason, UnavailableReason::NotApplicable)
    }

    /// Validate the optional detail bound.
    pub fn validate(&self) -> Result<(), ExactError> {
        if let Some(detail) = &self.detail {
            validate_text(detail, MAX_TEXT_BYTES, "unavailable detail")?;
        }
        Ok(())
    }
}

/// A value that is either known or explicitly unavailable.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Availability<T> {
    /// A known value.
    Known {
        /// The value.
        value: T,
    },
    /// No value; the reason is typed.
    Unavailable {
        /// The typed unavailability.
        unavailable: Unavailable,
    },
}

impl<T> Availability<T> {
    /// Wrap a known value.
    pub fn known(value: T) -> Self {
        Self::Known { value }
    }

    /// Wrap an unavailable value.
    pub fn unavailable(reason: UnavailableReason, detail: impl Into<String>) -> Self {
        Self::Unavailable {
            unavailable: Unavailable::new(reason, detail),
        }
    }

    /// True for a known value.
    pub const fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }

    /// Borrow the known value.
    pub const fn as_known(&self) -> Option<&T> {
        match self {
            Self::Known { value } => Some(value),
            Self::Unavailable { .. } => None,
        }
    }

    /// Consume into the known value.
    pub fn into_known(self) -> Option<T> {
        match self {
            Self::Known { value } => Some(value),
            Self::Unavailable { .. } => None,
        }
    }

    /// The unavailability, when absent.
    pub const fn unavailable_reason(&self) -> Option<&Unavailable> {
        match self {
            Self::Known { .. } => None,
            Self::Unavailable { unavailable } => Some(unavailable),
        }
    }

    /// Map a known value, preserving absence.
    pub fn map<U>(self, function: impl FnOnce(T) -> U) -> Availability<U> {
        match self {
            Self::Known { value } => Availability::Known {
                value: function(value),
            },
            Self::Unavailable { unavailable } => Availability::Unavailable { unavailable },
        }
    }

    /// Fallible map; an error becomes a typed unavailable.
    pub fn try_map<U, E: std::fmt::Display>(
        self,
        function: impl FnOnce(T) -> Result<U, E>,
    ) -> Availability<U> {
        match self {
            Self::Known { value } => match function(value) {
                Ok(value) => Availability::Known { value },
                Err(error) => Availability::Unavailable {
                    unavailable: Unavailable::new(UnavailableReason::OutOfRange, error.to_string()),
                },
            },
            Self::Unavailable { unavailable } => Availability::Unavailable { unavailable },
        }
    }

    /// Combine two known values, otherwise return the first absence.
    pub fn zip<U>(self, other: Availability<U>) -> Availability<(T, U)> {
        match (self, other) {
            (Self::Known { value: left }, Availability::Known { value: right }) => {
                Availability::Known {
                    value: (left, right),
                }
            }
            (Self::Unavailable { unavailable }, _)
            | (_, Availability::Unavailable { unavailable }) => {
                Availability::Unavailable { unavailable }
            }
        }
    }

    /// Validate the absence detail.
    pub fn validate(&self) -> Result<(), ExactError> {
        if let Self::Unavailable { unavailable } = self {
            unavailable.validate()?;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------ term

/// One asserted term: a value plus the evidence or assumption that binds it.
///
/// A known value **must** carry a basis; an unavailable value does not need
/// one. This is enforced by [`Term::validate`] and by every analysis kernel.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Term<T> {
    /// The value or its typed absence.
    pub value: Availability<T>,
    /// Evidence or assumption; required for a known value.
    #[serde(default)]
    pub basis: Option<Basis>,
}

impl<T> Term<T> {
    /// A known value backed by provenance.
    pub fn evidence(value: T, provenance: Provenance) -> Self {
        Self {
            value: Availability::known(value),
            basis: Some(Basis::Provenance(provenance)),
        }
    }

    /// A known value backed by an explicit assumption.
    pub fn assumption(value: T, assumption: Assumption) -> Self {
        Self {
            value: Availability::known(value),
            basis: Some(Basis::Assumption(assumption)),
        }
    }

    /// A known value that already carries a basis.
    pub fn with_basis(value: T, basis: Basis) -> Self {
        Self {
            value: Availability::known(value),
            basis: Some(basis),
        }
    }

    /// An unknown value.
    pub fn unknown(reason: UnavailableReason, detail: impl Into<String>) -> Self {
        Self {
            value: Availability::unavailable(reason, detail),
            basis: None,
        }
    }

    /// A term that does not apply, explicitly declared.
    pub fn not_applicable(detail: impl Into<String>) -> Self {
        Self::unknown(UnavailableReason::NotApplicable, detail)
    }

    /// True when the value is known.
    pub const fn is_known(&self) -> bool {
        self.value.is_known()
    }

    /// Borrow the known value.
    pub const fn as_known(&self) -> Option<&T> {
        self.value.as_known()
    }

    /// The basis, when present.
    pub const fn basis(&self) -> Option<&Basis> {
        self.basis.as_ref()
    }

    /// Validate the mandatory basis and the absence detail.
    pub fn validate(&self) -> Result<(), ExactError> {
        self.value.validate()?;
        if self.value.is_known() && self.basis.is_none() {
            return Err(ExactError::OutOfBounds {
                what: "term",
                detail: "a known value must carry evidence or an assumption".to_string(),
            });
        }
        if let Some(Basis::Provenance(provenance)) = &self.basis {
            provenance.validate()?;
        }
        if let Some(Basis::Assumption(assumption)) = &self.basis {
            assumption.validate()?;
        }
        Ok(())
    }

    /// Map the known value. The basis is preserved; an absent value stays
    /// absent with its reason.
    pub fn map<U>(self, function: impl FnOnce(T) -> U) -> Term<U> {
        Term {
            value: self.value.map(function),
            basis: self.basis,
        }
    }

    /// Fallible map. An arithmetic failure becomes a typed absence and the
    /// basis is dropped because no known value remains.
    pub fn try_map<U, E: std::fmt::Display>(
        self,
        function: impl FnOnce(T) -> Result<U, E>,
    ) -> Term<U> {
        match self.value {
            Availability::Known { value } => match function(value) {
                Ok(value) => Term {
                    value: Availability::Known { value },
                    basis: self.basis,
                },
                Err(error) => Term {
                    value: Availability::unavailable(
                        UnavailableReason::Arithmetic,
                        error.to_string(),
                    ),
                    basis: None,
                },
            },
            Availability::Unavailable { unavailable } => Term {
                value: Availability::Unavailable { unavailable },
                basis: self.basis,
            },
        }
    }
}

/// The kind of uncertainty in a decision input or result.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyClass {
    /// Exact integer value.
    Exact,
    /// Bounded interval.
    BoundedRange,
    /// Empirical distribution.
    Distribution,
    /// Deterministic approximation of a stochastic quantity.
    Approximation,
    /// No estimate exists.
    Unknown,
}

/// One uncertainty disclosure.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UncertaintyNote {
    /// The quantity concerned.
    pub subject: String,
    /// The uncertainty class.
    pub class: UncertaintyClass,
    /// Bounded explanation of the source and size of the uncertainty.
    pub note: String,
}

impl UncertaintyNote {
    /// Validate the note.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.subject, MAX_ID_BYTES, "uncertainty subject")?;
        validate_text(&self.note, MAX_TEXT_BYTES, "uncertainty note")
    }
}

/// A surfaced conflict. Values are never averaged into a synthetic truth.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conflict {
    /// The contested subject.
    pub subject: String,
    /// The competing asserted values, as bounded text.
    pub values: Vec<String>,
    /// Evidence supporting the competing claims.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// Why the conflict matters to the decision.
    pub impact: String,
}

impl Conflict {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), ExactError> {
        validate_text(&self.subject, MAX_ID_BYTES, "conflict subject")?;
        validate_text(&self.impact, MAX_TEXT_BYTES, "conflict impact")?;
        if self.values.len() < 2 || self.values.len() > MAX_CONDITIONS {
            return Err(ExactError::OutOfBounds {
                what: "conflict values",
                detail: format!("{} entries", self.values.len()),
            });
        }
        for value in &self.values {
            validate_text(value, MAX_TEXT_BYTES, "conflict value")?;
        }
        for evidence in &self.evidence {
            evidence.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provenance() -> Provenance {
        Provenance::new(
            SourceKind::TestReport,
            EvidenceRef::new("tr-1", Revision(3), Some("abc")).unwrap(),
            Day(20_000),
            "report line 4",
            vec!["lot L1 only".to_string()],
            "a repeat measurement outside tolerance",
        )
        .unwrap()
    }

    #[test]
    fn known_term_requires_basis() {
        let unbound = Term {
            value: Availability::known(5_u32),
            basis: None,
        };
        assert!(unbound.validate().is_err());
        let bound = Term::evidence(5_u32, provenance());
        assert!(bound.validate().is_ok());
        assert_eq!(bound.as_known(), Some(&5));
    }

    #[test]
    fn unknown_is_typed_and_distinct_from_zero() {
        let unknown: Term<u32> = Term::unknown(UnavailableReason::MissingEvidence, "no quote");
        assert!(!unknown.is_known());
        assert_eq!(
            unknown.value.unavailable_reason().map(|u| u.reason),
            Some(UnavailableReason::MissingEvidence)
        );
        let not_applicable: Term<u32> = Term::not_applicable("no duty for domestic goods");
        assert!(not_applicable
            .value
            .unavailable_reason()
            .unwrap()
            .is_not_applicable());
    }

    #[test]
    fn assumption_carries_falsifier_and_provenance() {
        let assumption = Assumption::new(
            "a1",
            "freight rate holds for 30 days",
            "carrier quote was captured today",
            provenance(),
            "a carrier re-quote above the assumed rate",
        )
        .unwrap();
        assert!(assumption.validate().is_ok());
        let term = Term::assumption(10_u32, assumption);
        assert!(term.validate().is_ok());
    }

    #[test]
    fn empty_or_oversized_text_is_rejected() {
        assert!(EvidenceRef::new("", Revision(1), None).is_err());
        assert!(EvidenceRef::new(&"x".repeat(MAX_ID_BYTES + 1), Revision(1), None).is_err());
        let conflict = Conflict {
            subject: "unit price".to_string(),
            values: vec!["1".to_string()],
            evidence: vec![],
            impact: "cannot compare".to_string(),
        };
        assert!(conflict.validate().is_err());
    }

    #[test]
    fn availability_map_preserves_absence() {
        let known: Availability<u32> = Availability::known(2);
        assert_eq!(known.map(|v| v * 3).as_known(), Some(&6));
        let absent: Availability<u32> = Availability::unavailable(UnavailableReason::Ambiguous, "");
        assert_eq!(
            absent.map(|v| v * 3).unavailable_reason().unwrap().reason,
            UnavailableReason::Ambiguous
        );
    }
}
