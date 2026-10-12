//! @authority-collapse: authority-bearing
//!
//! Intelligence-tier model (`docs/specs/autonomous-engineering.md`).
//!
//! Faktor is STANDALONE: it always knows which intelligence tier is
//! available locally and reports confidence honestly. No tier, capability or
//! confidence value here ever depends on Tangerine or any other external
//! provider — an external provider is at most ONE optional source of a
//! declared tier and declared evidence dimensions, and its absence simply
//! leaves the native Tier 0 truth in place.
//!
//! | tier | what it is                              | honest example                       |
//! |------|-----------------------------------------|--------------------------------------|
//! | 0    | filesystem + text search                | native fallback (walks, greps)       |
//! | 1    | syntax parsing                          | tree-sitter outlines                 |
//! | 2    | LSP / compiler semantics                | definitions/references from an LSP   |
//! | 3    | static dependency / call graph          | whole-program static graph           |
//! | 4    | runtime + historical evidence           | traces, coverage, past failures      |
//! | 5    | enriched external model                 | an external semantic provider        |
//!
//! Tier describes CAPABILITY (which machinery exists), never truth. It is
//! reported verbatim as metadata and is NOT an input to confidence: there is
//! no additive tier term, so a declared tier can never outrank a better
//! evidence claim.
//!
//! Confidence derives ONLY from orthogonal evidence dimensions:
//!
//! - **fidelity** — how close the answer is to real compiler semantics;
//! - **completeness** — how much of the truth the answer covers;
//! - **freshness** — whether the evidence describes the current material
//!   state or an older one;
//! - **determinism** — whether replaying the same inputs reproduces the
//!   same answer;
//! - **revision binding** — whether the evidence is pinned to the exact
//!   source revision the claim is about;
//! - **evidence provenance** — the scope of the evidence relative to the
//!   claim and the residual uncertainty the evidence declares;
//! - **measured corroboration** — independent agreeing measurements; the
//!   only axis that can raise confidence above the declared dimensions.
//!
//! Honesty rules enforced here:
//!
//! - The generic fallback is Tier 0 for context/delta/affected/explain
//!   (bounded searches plus heuristics); it never claims a tier or a
//!   capability it cannot deliver, and unavailable operations still report
//!   their true tier with zero confidence.
//! - An UNDECLARED fidelity ([`SemanticFidelity::None`]) pins confidence to
//!   zero: a tier can never create an evidence claim that was not made.
//! - Every dimension has a conservative `Unknown` level: absent declarations
//!   never raise confidence, and a compiler/deterministic exact fact
//!   outranks an external heuristic opinion at equal declared fidelity.

use serde::{Deserialize, Serialize};

use crate::fallback::GENERIC_FALLBACK_ID;
use crate::types::{
    SemanticCompleteness, SemanticError, SemanticFidelity, SemanticOp, SemanticOperationStatus,
};

/// The highest valid intelligence tier (tiers are `0..=5`).
pub const MAX_INTELLIGENCE_TIER: u8 = 5;

/// A validated intelligence tier (`0..=5`). Out-of-range values are a typed
/// refusal at construction, so an impossible tier can never reach a status
/// surface. A tier describes CAPABILITY metadata only: it is never scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IntelligenceTier(u8);

impl IntelligenceTier {
    /// Tier 0: filesystem + text search (the native fallback's honesty).
    pub const FILESYSTEM: Self = Self(0);
    /// Tier 1: syntax parsing.
    pub const SYNTAX: Self = Self(1);
    /// Tier 2: LSP / compiler semantics.
    pub const COMPILER: Self = Self(2);
    /// Tier 3: static dependency / call graph.
    pub const GRAPH: Self = Self(3);
    /// Tier 4: runtime + historical evidence.
    pub const RUNTIME: Self = Self(4);
    /// Tier 5: enriched external model.
    pub const ENRICHED: Self = Self(5);

    /// Parse a raw tier, refusing anything outside `0..=5` with a typed
    /// error. This is the ONLY constructor: `Some(6)` can never become an
    /// `IntelligenceTier`.
    pub fn parse(raw: u8) -> Result<Self, SemanticError> {
        if raw > MAX_INTELLIGENCE_TIER {
            return Err(SemanticError::InvalidIntelligenceTier { tier: raw });
        }
        Ok(Self(raw))
    }

    /// The raw tier value (`0..=5`).
    pub const fn raw(self) -> u8 {
        self.0
    }
}

impl Default for IntelligenceTier {
    /// The honest default is Tier 0: filesystem + text search. Installed
    /// external providers never widen this implicitly.
    fn default() -> Self {
        Self::FILESYSTEM
    }
}

/// Freshness of the evidence relative to the material state it describes.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceFreshness {
    /// Not declared: conservative, never assumed current.
    #[default]
    Unknown,
    /// Describes an older material state; drift from the current state is
    /// possible.
    Aged,
    /// Describes the current material state.
    Current,
}

/// Determinism of the evidence method: whether replaying the same inputs
/// reproduces the same answer.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceDeterminism {
    /// Not declared: conservative, never assumed reproducible.
    #[default]
    Unknown,
    /// An opinion/inference that is not reproducible from the same inputs.
    Heuristic,
    /// A deterministic engine: identical inputs replay the identical answer.
    Deterministic,
}

/// How tightly the evidence is bound to a source revision.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RevisionBinding {
    /// Not declared: conservative.
    #[default]
    Unknown,
    /// Associated with the source, but not pinned to a revision identity.
    Inferred,
    /// Pinned to the exact source revision the claim is about.
    Exact,
}

/// The scope of the evidence relative to the claim.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceScope {
    /// Not declared: conservative.
    #[default]
    Unknown,
    /// Evidence about neighboring material; the claim is an extrapolation.
    Adjacent,
    /// Evidence about the exact claim.
    Direct,
}

/// The residual uncertainty the evidence declares.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceUncertainty {
    /// Not declared: conservative — absence of a disclosure is NOT an
    /// all-clear.
    #[default]
    Unknown,
    /// Residual uncertainty is explicitly disclosed.
    Declared,
    /// The evidence explicitly claims a complete determination with no
    /// residual uncertainty.
    None,
}

/// Measured independent corroboration: how many independent, agreeing
/// measurements stand behind the evidence.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCorroboration {
    /// Not declared: no uplift.
    #[default]
    Unknown,
    /// Explicitly no independent confirmation: no uplift.
    Uncorroborated,
    /// Measured agreement from independent sources. The uplift saturates at
    /// [`MAX_COUNTED_CORROBORATION`] sources; the count is a measurement,
    /// never a tier claim.
    Measured { independent_sources: u8 },
}

/// The largest number of independent corroborating sources that raises
/// confidence; further sources are counted but add nothing (bounded uplift).
pub const MAX_COUNTED_CORROBORATION: u8 = 4;

/// Evidence provenance: the scope of the evidence relative to the claim plus
/// the residual uncertainty it declares.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EvidenceProvenance {
    #[serde(default)]
    pub scope: EvidenceScope,
    #[serde(default)]
    pub uncertainty: EvidenceUncertainty,
}

/// The orthogonal evidence dimensions beyond fidelity/completeness that feed
/// confidence. Every axis defaults to its conservative `Unknown`, so an
/// absent declaration never raises confidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvidenceDimensions {
    #[serde(default)]
    pub freshness: EvidenceFreshness,
    #[serde(default)]
    pub determinism: EvidenceDeterminism,
    #[serde(default)]
    pub revision_binding: RevisionBinding,
    #[serde(default)]
    pub provenance: EvidenceProvenance,
    #[serde(default)]
    pub corroboration: EvidenceCorroboration,
}

impl EvidenceDimensions {
    /// The conservative all-`Unknown` dimensions: nothing was declared.
    pub const UNKNOWN: Self = Self {
        freshness: EvidenceFreshness::Unknown,
        determinism: EvidenceDeterminism::Unknown,
        revision_binding: RevisionBinding::Unknown,
        provenance: EvidenceProvenance {
            scope: EvidenceScope::Unknown,
            uncertainty: EvidenceUncertainty::Unknown,
        },
        corroboration: EvidenceCorroboration::Unknown,
    };

    /// TRUE when no axis was declared (the conservative default).
    pub const fn is_undeclared(&self) -> bool {
        matches!(self.freshness, EvidenceFreshness::Unknown)
            && matches!(self.determinism, EvidenceDeterminism::Unknown)
            && matches!(self.revision_binding, RevisionBinding::Unknown)
            && matches!(self.provenance.scope, EvidenceScope::Unknown)
            && matches!(self.provenance.uncertainty, EvidenceUncertainty::Unknown)
            && matches!(self.corroboration, EvidenceCorroboration::Unknown)
    }
}

/// The complete set of orthogonal dimensions confidence derives from.
/// Deliberately contains NO tier: tier describes capability, not truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfidenceDimensions {
    pub fidelity: SemanticFidelity,
    pub completeness: SemanticCompleteness,
    #[serde(default)]
    pub freshness: EvidenceFreshness,
    #[serde(default)]
    pub determinism: EvidenceDeterminism,
    #[serde(default)]
    pub revision_binding: RevisionBinding,
    #[serde(default)]
    pub provenance: EvidenceProvenance,
    #[serde(default)]
    pub corroboration: EvidenceCorroboration,
}

impl ConfidenceDimensions {
    /// Combine a fidelity/completeness claim with the remaining orthogonal
    /// evidence dimensions.
    pub const fn new(
        fidelity: SemanticFidelity,
        completeness: SemanticCompleteness,
        evidence: EvidenceDimensions,
    ) -> Self {
        Self {
            fidelity,
            completeness,
            freshness: evidence.freshness,
            determinism: evidence.determinism,
            revision_binding: evidence.revision_binding,
            provenance: evidence.provenance,
            corroboration: evidence.corroboration,
        }
    }
}

/// Fidelity base (bps).
fn fidelity_base(fidelity: SemanticFidelity) -> u32 {
    match fidelity {
        SemanticFidelity::None => 0,
        SemanticFidelity::Heuristic => 2_500,
        SemanticFidelity::Structural => 5_000,
        SemanticFidelity::CompilerExact => 8_000,
        SemanticFidelity::Proof => 9_500,
    }
}

/// Completeness factor (bps).
fn completeness_factor(completeness: SemanticCompleteness) -> u32 {
    match completeness {
        SemanticCompleteness::Unknown => 5_000,
        SemanticCompleteness::Partial => 7_500,
        SemanticCompleteness::ConservativeComplete => 9_000,
        SemanticCompleteness::Complete => 10_000,
    }
}

/// Freshness factor (bps).
fn freshness_factor(freshness: EvidenceFreshness) -> u32 {
    match freshness {
        EvidenceFreshness::Unknown => 7_000,
        EvidenceFreshness::Aged => 8_500,
        EvidenceFreshness::Current => 10_000,
    }
}

/// Determinism factor (bps).
fn determinism_factor(determinism: EvidenceDeterminism) -> u32 {
    match determinism {
        EvidenceDeterminism::Unknown => 7_000,
        EvidenceDeterminism::Heuristic => 8_500,
        EvidenceDeterminism::Deterministic => 10_000,
    }
}

/// Revision-binding factor (bps).
fn revision_binding_factor(binding: RevisionBinding) -> u32 {
    match binding {
        RevisionBinding::Unknown => 7_000,
        RevisionBinding::Inferred => 8_500,
        RevisionBinding::Exact => 10_000,
    }
}

/// Provenance-scope factor (bps).
fn scope_factor(scope: EvidenceScope) -> u32 {
    match scope {
        EvidenceScope::Unknown => 7_000,
        EvidenceScope::Adjacent => 8_500,
        EvidenceScope::Direct => 10_000,
    }
}

/// Declared-uncertainty factor (bps).
fn uncertainty_factor(uncertainty: EvidenceUncertainty) -> u32 {
    match uncertainty {
        EvidenceUncertainty::Unknown => 8_000,
        EvidenceUncertainty::Declared => 9_000,
        EvidenceUncertainty::None => 10_000,
    }
}

/// Measured-corroboration uplift factor (bps): neutral without a measurement,
/// up to `+3000` bps for [`MAX_COUNTED_CORROBORATION`] or more independent
/// agreeing sources.
fn corroboration_factor(corroboration: EvidenceCorroboration) -> u32 {
    match corroboration {
        EvidenceCorroboration::Unknown | EvidenceCorroboration::Uncorroborated => 10_000,
        EvidenceCorroboration::Measured {
            independent_sources,
        } => {
            let counted = if independent_sources > MAX_COUNTED_CORROBORATION {
                MAX_COUNTED_CORROBORATION
            } else {
                independent_sources
            };
            10_000 + u32::from(counted) * 750
        }
    }
}

/// The confidence mapping in basis points (`0..=10_000`).
///
/// ```text
/// confidence = clamp(fidelity_base
///                    × completeness × freshness × determinism
///                    × revision_binding × scope × uncertainty
///                    × corroboration
///                    / 10_000^7, 0, 10_000)
/// ```
///
/// An undeclared fidelity ([`SemanticFidelity::None`]) pins the result to
/// `0`. There is deliberately NO tier term: every factor is an axis of the
/// evidence itself, so a compiler/deterministic exact fact outranks an
/// external heuristic opinion at equal declared fidelity and a tier claim
/// can never launder an evidence-quality claim that was not made.
pub fn confidence_bps_for(dimensions: &ConfidenceDimensions) -> u16 {
    if dimensions.fidelity == SemanticFidelity::None {
        // No declared evidence quality: confidence stays 0 regardless of
        // completeness, tier or any other dimension.
        return 0;
    }
    let product = u128::from(fidelity_base(dimensions.fidelity))
        * u128::from(completeness_factor(dimensions.completeness))
        * u128::from(freshness_factor(dimensions.freshness))
        * u128::from(determinism_factor(dimensions.determinism))
        * u128::from(revision_binding_factor(dimensions.revision_binding))
        * u128::from(scope_factor(dimensions.provenance.scope))
        * u128::from(uncertainty_factor(dimensions.provenance.uncertainty))
        * u128::from(corroboration_factor(dimensions.corroboration));
    (product / 10_000u128.pow(7)).min(10_000) as u16
}

/// The typed per-operation capability projection: [`IntelligenceCapability`]
/// reuses/absorbs [`SemanticOperationStatus`] (existing consumers keep the
/// old struct) and adds the tier metadata plus the orthogonal confidence
/// dimensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntelligenceCapability {
    pub op: SemanticOp,
    /// The declared tier, or `None` when no tier was declared (unknown,
    /// never assumed high). Capability metadata only: never scored.
    pub tier: Option<IntelligenceTier>,
    pub available: bool,
    /// The orthogonal evidence dimensions confidence derives from. The
    /// fidelity/completeness axes mirror the operation status; the remaining
    /// axes come from the declared evidence dimensions (absent = unknown).
    pub dimensions: ConfidenceDimensions,
    pub source: String,
}

impl IntelligenceCapability {
    /// Project an existing [`SemanticOperationStatus`] plus a tier and the
    /// remaining declared evidence dimensions into the intelligence surface,
    /// without changing the status type its consumers already use.
    pub fn from_status(
        op: SemanticOp,
        status: SemanticOperationStatus,
        tier: Option<IntelligenceTier>,
        evidence: EvidenceDimensions,
    ) -> Self {
        Self {
            op,
            tier,
            available: status.available,
            dimensions: ConfidenceDimensions::new(status.fidelity, status.completeness, evidence),
            source: status.source,
        }
    }

    /// The declared evidence fidelity.
    pub const fn fidelity(&self) -> SemanticFidelity {
        self.dimensions.fidelity
    }

    /// The declared completeness.
    pub const fn completeness(&self) -> SemanticCompleteness {
        self.dimensions.completeness
    }

    /// The bounded confidence (`0..=10_000`) for this capability. An
    /// unavailable operation always reports `0`: a capability that is not
    /// served cannot carry confidence.
    pub fn confidence_bps(&self) -> u16 {
        if !self.available {
            return 0;
        }
        confidence_bps_for(&self.dimensions)
    }
}

/// The honest native evidence dimensions: a bounded, deterministic read-only
/// pass over the CURRENT workspace, scoped directly to the request, bound by
/// inference (no revision-hash pin) and disclosing residual uncertainty
/// through its bounded completeness.
pub const fn native_evidence_dimensions() -> EvidenceDimensions {
    EvidenceDimensions {
        freshness: EvidenceFreshness::Current,
        determinism: EvidenceDeterminism::Deterministic,
        revision_binding: RevisionBinding::Inferred,
        provenance: EvidenceProvenance {
            scope: EvidenceScope::Direct,
            uncertainty: EvidenceUncertainty::Declared,
        },
        corroboration: EvidenceCorroboration::Unknown,
    }
}

/// The generic fallback's honest per-operation intelligence capability: Tier
/// 0 (filesystem + text search), heuristic fidelity, its real completeness
/// and source, exactly as [`crate::registry::fallback_operation_status`]
/// reports — with the tier made explicit instead of implied and the native
/// evidence dimensions declared.
pub fn fallback_intelligence_capability(op: SemanticOp) -> IntelligenceCapability {
    IntelligenceCapability::from_status(
        op,
        crate::registry::fallback_operation_status(op),
        Some(IntelligenceTier::FILESYSTEM),
        native_evidence_dimensions(),
    )
}

/// The fallback's honest per-operation capabilities for every semantic op.
pub fn fallback_intelligence_capabilities() -> Vec<IntelligenceCapability> {
    SemanticOp::ALL
        .iter()
        .map(|op| fallback_intelligence_capability(*op))
        .collect()
}

/// The highest tier among AVAILABLE capabilities, or `None` when nothing is
/// available. Unavailable operations never raise this value.
pub fn highest_available_tier(capabilities: &[IntelligenceCapability]) -> Option<IntelligenceTier> {
    capabilities
        .iter()
        .filter(|capability| capability.available)
        .filter_map(|capability| capability.tier)
        .max()
}

/// Bounded ceiling of the native service entries [`native_capabilities`]
/// reports. Kept as a constant so the report is provably bounded, not merely
/// small today.
pub const MAX_NATIVE_INTELLIGENCE_SERVICES: usize = 8;

/// One native (no external provider) intelligence service. Data-only: it
/// describes what Faktor's own filesystem/index/workspace machinery can
/// honestly answer, never a synthesized symbol graph or compiler claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeIntelligenceService {
    pub name: String,
    pub tier: IntelligenceTier,
    pub available: bool,
    pub fidelity: SemanticFidelity,
    pub completeness: SemanticCompleteness,
    /// The service's declared orthogonal evidence dimensions.
    pub evidence: EvidenceDimensions,
}

impl NativeIntelligenceService {
    /// The bounded confidence for this service under the shared mapping.
    pub fn confidence_bps(&self) -> u16 {
        if !self.available {
            return 0;
        }
        confidence_bps_for(&ConfidenceDimensions::new(
            self.fidelity,
            self.completeness,
            self.evidence,
        ))
    }
}

/// The bounded native capability report: Tier 0 services Faktor can deliver
/// without any external provider, plus the fallback's per-op capability
/// statuses. Data-only — it feeds status surfaces and is never an operation
/// path itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeIntelligenceReport {
    /// The native report is always Tier 0: filesystem + text search.
    pub tier: IntelligenceTier,
    pub source: String,
    pub services: Vec<NativeIntelligenceService>,
    pub operations: Vec<IntelligenceCapability>,
}

/// The native capabilities Faktor can honestly provide using its existing
/// machinery — file discovery, text search and the bounded read-only
/// workspace pass — plus the fallback's capability-status entries for every
/// semantic op. No symbol graph, type system or compiler identity is
/// invented here; those require a higher tier and a provider that declares
/// it.
pub fn native_capabilities() -> NativeIntelligenceReport {
    let services = vec![
        NativeIntelligenceService {
            name: "file_discovery".to_string(),
            tier: IntelligenceTier::FILESYSTEM,
            available: true,
            fidelity: SemanticFidelity::Heuristic,
            completeness: SemanticCompleteness::ConservativeComplete,
            evidence: native_evidence_dimensions(),
        },
        NativeIntelligenceService {
            name: "text_search".to_string(),
            tier: IntelligenceTier::FILESYSTEM,
            available: true,
            fidelity: SemanticFidelity::Heuristic,
            completeness: SemanticCompleteness::Partial,
            evidence: native_evidence_dimensions(),
        },
        NativeIntelligenceService {
            name: "read_only_pass".to_string(),
            tier: IntelligenceTier::FILESYSTEM,
            available: true,
            fidelity: SemanticFidelity::Heuristic,
            completeness: SemanticCompleteness::Partial,
            evidence: native_evidence_dimensions(),
        },
    ];
    debug_assert!(services.len() <= MAX_NATIVE_INTELLIGENCE_SERVICES);
    NativeIntelligenceReport {
        tier: IntelligenceTier::FILESYSTEM,
        source: GENERIC_FALLBACK_ID.to_string(),
        services,
        operations: fallback_intelligence_capabilities(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SemanticProviderDescriptor;
    use crate::types::SemanticProviderId;

    /// Fully declared, all-good evidence: current, deterministic, exactly
    /// revision-bound, directly scoped, no residual uncertainty, no measured
    /// corroboration.
    const ALL_GOOD: EvidenceDimensions = EvidenceDimensions {
        freshness: EvidenceFreshness::Current,
        determinism: EvidenceDeterminism::Deterministic,
        revision_binding: RevisionBinding::Exact,
        provenance: EvidenceProvenance {
            scope: EvidenceScope::Direct,
            uncertainty: EvidenceUncertainty::None,
        },
        corroboration: EvidenceCorroboration::Unknown,
    };

    /// Nothing declared: every axis sits at its conservative `Unknown`.
    const ALL_UNKNOWN: EvidenceDimensions = EvidenceDimensions {
        freshness: EvidenceFreshness::Unknown,
        determinism: EvidenceDeterminism::Unknown,
        revision_binding: RevisionBinding::Unknown,
        provenance: EvidenceProvenance {
            scope: EvidenceScope::Unknown,
            uncertainty: EvidenceUncertainty::Unknown,
        },
        corroboration: EvidenceCorroboration::Unknown,
    };

    fn dimensions(
        fidelity: SemanticFidelity,
        completeness: SemanticCompleteness,
        evidence: EvidenceDimensions,
    ) -> ConfidenceDimensions {
        ConfidenceDimensions::new(fidelity, completeness, evidence)
    }

    fn capability(
        tier: Option<IntelligenceTier>,
        evidence: EvidenceDimensions,
        source: &str,
    ) -> IntelligenceCapability {
        IntelligenceCapability {
            op: SemanticOp::Context,
            tier,
            available: true,
            dimensions: dimensions(
                SemanticFidelity::CompilerExact,
                SemanticCompleteness::Complete,
                evidence,
            ),
            source: source.to_string(),
        }
    }

    #[test]
    fn tier_validation_refuses_out_of_range_typed() {
        for raw in 0..=MAX_INTELLIGENCE_TIER {
            assert_eq!(
                IntelligenceTier::parse(raw).expect("in-range tier").raw(),
                raw
            );
        }
        for raw in [6u8, 7, 100, u8::MAX] {
            match IntelligenceTier::parse(raw) {
                Err(SemanticError::InvalidIntelligenceTier { tier }) => assert_eq!(tier, raw),
                other => panic!("{raw} must be refused typed, got {other:?}"),
            }
        }
        assert_eq!(IntelligenceTier::default(), IntelligenceTier::FILESYSTEM);

        // The descriptor validates the tier at handshake: 0..=5 accepted,
        // 6 refused typed, and an absent tier stays unknown.
        let id = SemanticProviderId::parse("tier-probe").unwrap();
        let mut descriptor = SemanticProviderDescriptor::new(
            id,
            1,
            crate::types::SEMANTIC_SCHEMA_VERSION,
            crate::types::SemanticCapabilities::CONTEXT,
        );
        assert!(descriptor.validate().is_ok(), "absent tier is valid");
        assert_eq!(descriptor.intelligence_tier(), None);
        for raw in 0..=MAX_INTELLIGENCE_TIER {
            descriptor.tier = Some(raw);
            assert!(descriptor.validate().is_ok(), "tier {raw} must validate");
            assert_eq!(descriptor.intelligence_tier().unwrap().raw(), raw);
        }
        descriptor.tier = Some(MAX_INTELLIGENCE_TIER + 1);
        match descriptor.validate() {
            Err(SemanticError::InvalidIntelligenceTier { tier }) => {
                assert_eq!(tier, MAX_INTELLIGENCE_TIER + 1)
            }
            other => panic!("out-of-range tier must be refused typed, got {other:?}"),
        }
    }

    #[test]
    fn undeclared_fidelity_pins_confidence_to_zero() {
        use SemanticCompleteness as C;
        let evidence_cases = [
            ALL_UNKNOWN,
            ALL_GOOD,
            native_evidence_dimensions(),
            EvidenceDimensions {
                corroboration: EvidenceCorroboration::Measured {
                    independent_sources: u8::MAX,
                },
                ..ALL_GOOD
            },
        ];
        for completeness in [C::Unknown, C::Partial, C::ConservativeComplete, C::Complete] {
            for evidence in evidence_cases {
                assert_eq!(
                    confidence_bps_for(&dimensions(SemanticFidelity::None, completeness, evidence)),
                    0,
                    "an undeclared fidelity can never carry confidence"
                );
            }
        }
        // The same profile WITH a declared fidelity is positive: the zero
        // comes from the undeclared fidelity, not from the other axes.
        assert!(
            confidence_bps_for(&dimensions(
                SemanticFidelity::Heuristic,
                C::Complete,
                ALL_GOOD
            )) > 0
        );
    }

    #[test]
    fn confidence_is_exact_bounded_and_monotone_per_dimension() {
        use SemanticCompleteness as C;
        use SemanticFidelity as F;

        // Exact values: with all other axes all-good, confidence collapses
        // to fidelity × completeness.
        assert_eq!(
            confidence_bps_for(&dimensions(F::Heuristic, C::Complete, ALL_GOOD)),
            2_500
        );
        assert_eq!(
            confidence_bps_for(&dimensions(F::Structural, C::Complete, ALL_GOOD)),
            5_000
        );
        assert_eq!(
            confidence_bps_for(&dimensions(F::CompilerExact, C::Complete, ALL_GOOD)),
            8_000
        );
        assert_eq!(
            confidence_bps_for(&dimensions(F::Proof, C::Complete, ALL_GOOD)),
            9_500
        );
        // Absent evidence dimensions are conservative: 8000 × 0.7^4 × 0.8.
        assert_eq!(
            confidence_bps_for(&dimensions(F::CompilerExact, C::Complete, ALL_UNKNOWN)),
            1_536
        );
        // The honest native dimensions at heuristic/partial fidelity.
        assert_eq!(
            confidence_bps_for(&dimensions(
                F::Heuristic,
                C::Partial,
                native_evidence_dimensions()
            )),
            1_434
        );

        let fidelities = [
            F::None,
            F::Heuristic,
            F::Structural,
            F::CompilerExact,
            F::Proof,
        ];
        let completenesses = [C::Unknown, C::Partial, C::ConservativeComplete, C::Complete];
        let freshnesses = [
            EvidenceFreshness::Unknown,
            EvidenceFreshness::Aged,
            EvidenceFreshness::Current,
        ];
        let determinisms = [
            EvidenceDeterminism::Unknown,
            EvidenceDeterminism::Heuristic,
            EvidenceDeterminism::Deterministic,
        ];
        let bindings = [
            RevisionBinding::Unknown,
            RevisionBinding::Inferred,
            RevisionBinding::Exact,
        ];
        let scopes = [
            EvidenceScope::Unknown,
            EvidenceScope::Adjacent,
            EvidenceScope::Direct,
        ];
        let uncertainties = [
            EvidenceUncertainty::Unknown,
            EvidenceUncertainty::Declared,
            EvidenceUncertainty::None,
        ];
        let corroborations = [
            EvidenceCorroboration::Unknown,
            EvidenceCorroboration::Uncorroborated,
            EvidenceCorroboration::Measured {
                independent_sources: 0,
            },
            EvidenceCorroboration::Measured {
                independent_sources: 1,
            },
            EvidenceCorroboration::Measured {
                independent_sources: 2,
            },
            EvidenceCorroboration::Measured {
                independent_sources: 3,
            },
            EvidenceCorroboration::Measured {
                independent_sources: 4,
            },
            EvidenceCorroboration::Measured {
                independent_sources: u8::MAX,
            },
        ];

        // Bounded and monotone non-decreasing in every axis, over every
        // combination.
        for fidelity in fidelities {
            for completeness in completenesses {
                for freshness in freshnesses {
                    for determinism in determinisms {
                        for binding in bindings {
                            for scope in scopes {
                                for uncertainty in uncertainties {
                                    for corroboration in corroborations {
                                        let value = confidence_bps_for(&dimensions(
                                            fidelity,
                                            completeness,
                                            EvidenceDimensions {
                                                freshness,
                                                determinism,
                                                revision_binding: binding,
                                                provenance: EvidenceProvenance {
                                                    scope,
                                                    uncertainty,
                                                },
                                                corroboration,
                                            },
                                        ));
                                        assert!(value <= 10_000, "{value}");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let base = ALL_UNKNOWN;
        for pair in fidelities.windows(2) {
            assert!(
                confidence_bps_for(&dimensions(pair[0], C::Complete, base))
                    <= confidence_bps_for(&dimensions(pair[1], C::Complete, base)),
                "fidelity {pair:?} must not lower confidence"
            );
        }
        for pair in completenesses.windows(2) {
            assert!(
                confidence_bps_for(&dimensions(F::Heuristic, pair[0], base))
                    <= confidence_bps_for(&dimensions(F::Heuristic, pair[1], base)),
                "completeness {pair:?} must not lower confidence"
            );
        }
        for pair in freshnesses.windows(2) {
            assert!(
                confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        freshness: pair[0],
                        ..ALL_GOOD
                    }
                )) <= confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        freshness: pair[1],
                        ..ALL_GOOD
                    }
                )),
                "freshness {pair:?} must not lower confidence"
            );
        }
        for pair in determinisms.windows(2) {
            assert!(
                confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        determinism: pair[0],
                        ..ALL_GOOD
                    }
                )) <= confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        determinism: pair[1],
                        ..ALL_GOOD
                    }
                )),
                "determinism {pair:?} must not lower confidence"
            );
        }
        for pair in bindings.windows(2) {
            assert!(
                confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        revision_binding: pair[0],
                        ..ALL_GOOD
                    }
                )) <= confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        revision_binding: pair[1],
                        ..ALL_GOOD
                    }
                )),
                "revision binding {pair:?} must not lower confidence"
            );
        }
        for pair in corroborations.windows(2) {
            assert!(
                confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        corroboration: pair[0],
                        ..ALL_GOOD
                    }
                )) <= confidence_bps_for(&dimensions(
                    F::Heuristic,
                    C::Complete,
                    EvidenceDimensions {
                        corroboration: pair[1],
                        ..ALL_GOOD
                    }
                )),
                "corroboration {pair:?} must not lower confidence"
            );
        }
    }

    /// The P2 regression: with equal DECLARED fidelity and completeness, a
    /// native deterministic exact fact must outrank an external heuristic
    /// opinion — no tier term can flip it.
    #[test]
    fn native_exact_fact_outscores_external_heuristic_opinion_at_equal_fidelity() {
        let native = capability(Some(IntelligenceTier::FILESYSTEM), ALL_GOOD, "native");
        let external = capability(
            Some(IntelligenceTier::ENRICHED),
            EvidenceDimensions {
                freshness: EvidenceFreshness::Aged,
                determinism: EvidenceDeterminism::Heuristic,
                revision_binding: RevisionBinding::Inferred,
                provenance: EvidenceProvenance {
                    scope: EvidenceScope::Adjacent,
                    uncertainty: EvidenceUncertainty::Declared,
                },
                corroboration: EvidenceCorroboration::Unknown,
            },
            "external.heuristic",
        );
        assert_eq!(native.dimensions.fidelity, external.dimensions.fidelity);
        assert_eq!(
            native.dimensions.completeness,
            external.dimensions.completeness
        );
        assert_eq!(native.confidence_bps(), 8_000);
        assert_eq!(external.confidence_bps(), 3_758);
        assert!(
            native.confidence_bps() > external.confidence_bps(),
            "a native exact fact must outrank an external heuristic opinion at equal fidelity"
        );
        // The external's Tier 5 capability claim is reported metadata and
        // lifts nothing.
        assert_eq!(external.tier, Some(IntelligenceTier::ENRICHED));
    }

    #[test]
    fn determinism_and_revision_binding_raise_confidence() {
        use SemanticCompleteness as C;
        use SemanticFidelity as F;
        let det = |determinism| {
            confidence_bps_for(&dimensions(
                F::Heuristic,
                C::Complete,
                EvidenceDimensions {
                    determinism,
                    ..ALL_GOOD
                },
            ))
        };
        assert!(det(EvidenceDeterminism::Unknown) < det(EvidenceDeterminism::Heuristic));
        assert!(det(EvidenceDeterminism::Heuristic) < det(EvidenceDeterminism::Deterministic));

        let binding = |revision_binding| {
            confidence_bps_for(&dimensions(
                F::Heuristic,
                C::Complete,
                EvidenceDimensions {
                    revision_binding,
                    ..ALL_GOOD
                },
            ))
        };
        assert!(binding(RevisionBinding::Unknown) < binding(RevisionBinding::Inferred));
        assert!(binding(RevisionBinding::Inferred) < binding(RevisionBinding::Exact));

        // The exact values: unknown determinism/binding are conservative.
        assert_eq!(det(EvidenceDeterminism::Unknown), 1_750);
        assert_eq!(det(EvidenceDeterminism::Heuristic), 2_125);
        assert_eq!(det(EvidenceDeterminism::Deterministic), 2_500);
        assert_eq!(binding(RevisionBinding::Unknown), 1_750);
        assert_eq!(binding(RevisionBinding::Inferred), 2_125);
        assert_eq!(binding(RevisionBinding::Exact), 2_500);
    }

    /// Actual measured corroboration raises confidence and can lift an
    /// external heuristic capability above the native heuristic fallback at
    /// equal declared fidelity — corroboration is measured, never a tier
    /// claim.
    #[test]
    fn measured_corroboration_can_outrank_native_heuristic() {
        use SemanticCompleteness as C;
        use SemanticFidelity as F;
        let native = fallback_intelligence_capability(SemanticOp::Context);
        assert_eq!(native.confidence_bps(), 1_434);

        let corroborated = |sources: u8| IntelligenceCapability {
            op: SemanticOp::Context,
            tier: Some(IntelligenceTier::ENRICHED),
            available: true,
            dimensions: dimensions(
                F::Heuristic,
                C::Partial,
                EvidenceDimensions {
                    corroboration: EvidenceCorroboration::Measured {
                        independent_sources: sources,
                    },
                    ..native_evidence_dimensions()
                },
            ),
            source: "external.corroborated".to_string(),
        };

        let zero = corroborated(0);
        assert_eq!(
            zero.confidence_bps(),
            native.confidence_bps(),
            "zero measured sources must not raise anything"
        );
        let mut previous = zero.confidence_bps();
        for sources in [1u8, 2, 3, 4] {
            let current = corroborated(sources).confidence_bps();
            assert!(
                current > previous,
                "measured corroboration must strictly raise confidence: {sources}"
            );
            previous = current;
        }
        // Saturation: sources beyond the bound add nothing.
        assert_eq!(corroborated(u8::MAX).confidence_bps(), previous);
        assert!(
            corroborated(MAX_COUNTED_CORROBORATION).confidence_bps() > native.confidence_bps(),
            "corroborated external evidence can outrank the native heuristic"
        );
    }

    /// No confidence term is a function of tier: identical dimensions with
    /// different tier claims score identically, and the highest tier adds
    /// nothing.
    #[test]
    fn no_confidence_term_depends_on_tier() {
        let tiers = [
            None,
            Some(IntelligenceTier::FILESYSTEM),
            Some(IntelligenceTier::SYNTAX),
            Some(IntelligenceTier::COMPILER),
            Some(IntelligenceTier::GRAPH),
            Some(IntelligenceTier::RUNTIME),
            Some(IntelligenceTier::ENRICHED),
        ];
        let mut scored = Vec::new();
        for tier in tiers {
            let scored_capability = capability(tier, ALL_UNKNOWN, "tier-probe");
            scored.push((tier, scored_capability.confidence_bps()));
        }
        assert!(
            scored.windows(2).all(|pair| pair[0].1 == pair[1].1),
            "tier must never move confidence: {scored:?}"
        );
        // The boundary shape from the P2 report: a tier-5 external claim on
        // conservative evidence stays strictly below a tier-0 native exact
        // fact with all dimensions declared.
        assert!(
            capability(Some(IntelligenceTier::ENRICHED), ALL_UNKNOWN, "external").confidence_bps()
                < capability(Some(IntelligenceTier::FILESYSTEM), ALL_GOOD, "native")
                    .confidence_bps()
        );
    }

    #[test]
    fn fallback_reports_tier_zero_native_dimensions_and_positive_confidence() {
        use SemanticCompleteness as C;
        use SemanticFidelity as F;
        let native = native_evidence_dimensions();
        for (op, completeness, expected) in [
            (SemanticOp::Context, C::Partial, 1_434u16),
            (SemanticOp::Explain, C::Partial, 1_434),
            (SemanticOp::Delta, C::Unknown, 956),
            (SemanticOp::Affected, C::Unknown, 956),
        ] {
            let capability = fallback_intelligence_capability(op);
            assert!(capability.available, "{op:?}");
            assert_eq!(capability.tier, Some(IntelligenceTier::FILESYSTEM));
            assert_eq!(capability.fidelity(), F::Heuristic);
            assert_eq!(capability.completeness(), completeness);
            assert_eq!(capability.dimensions.freshness, native.freshness);
            assert_eq!(capability.dimensions.determinism, native.determinism);
            assert_eq!(
                capability.dimensions.revision_binding,
                native.revision_binding
            );
            assert_eq!(capability.dimensions.provenance, native.provenance);
            assert_eq!(capability.source, GENERIC_FALLBACK_ID);
            assert_eq!(capability.confidence_bps(), expected, "{op:?}");
        }
        // The fallback never claims what it cannot deliver; even the
        // unavailable operations report their true tier with zero
        // confidence.
        for op in [SemanticOp::Snapshot, SemanticOp::Verify] {
            let capability = fallback_intelligence_capability(op);
            assert!(!capability.available, "{op:?}");
            assert_eq!(capability.tier, Some(IntelligenceTier::FILESYSTEM));
            assert_eq!(capability.confidence_bps(), 0, "{op:?}");
        }
        let capabilities = fallback_intelligence_capabilities();
        assert_eq!(capabilities.len(), SemanticOp::ALL.len());
        assert_eq!(
            highest_available_tier(&capabilities),
            Some(IntelligenceTier::FILESYSTEM)
        );
        assert_eq!(highest_available_tier(&[]), None);
    }

    #[test]
    fn native_capabilities_are_bounded_data_only() {
        let report = native_capabilities();
        assert_eq!(report.tier, IntelligenceTier::FILESYSTEM);
        assert_eq!(report.source, GENERIC_FALLBACK_ID);
        assert!(
            report.services.len() <= MAX_NATIVE_INTELLIGENCE_SERVICES,
            "the native report is bounded"
        );
        let names: Vec<&str> = report
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["file_discovery", "text_search", "read_only_pass"]
        );
        for service in &report.services {
            assert!(service.available);
            assert_eq!(service.tier, IntelligenceTier::FILESYSTEM);
            assert_eq!(service.fidelity, SemanticFidelity::Heuristic);
            assert_eq!(service.evidence, native_evidence_dimensions());
            assert!(service.confidence_bps() <= 10_000);
        }
        assert_eq!(
            report.services[0].confidence_bps(),
            1_721,
            "file_discovery is conservative-complete"
        );
        assert_eq!(report.services[1].confidence_bps(), 1_434);
        assert_eq!(report.operations.len(), SemanticOp::ALL.len());
        assert!(report
            .operations
            .iter()
            .all(|capability| capability.tier == Some(IntelligenceTier::FILESYSTEM)));
    }

    #[test]
    fn descriptor_tier_is_additively_optional_on_the_wire() {
        // A descriptor encoded by an OLD provider (no tier, no evidence)
        // decodes with `None`/all-unknown, and a descriptor with `tier` and
        // `evidence` decodes with them set.
        let without: SemanticProviderDescriptor = serde_json::from_str(
            r#"{
                "id": "old-provider",
                "version": 1,
                "schema_version": 1,
                "capabilities": {
                    "ops": 2,
                    "compose_delta": false,
                    "constrained_edit": false
                }
            }"#,
        )
        .expect("a descriptor without a tier must decode additively");
        assert_eq!(without.tier, None);
        assert_eq!(without.intelligence_tier(), None);
        assert!(
            without.evidence.is_undeclared(),
            "absent evidence axes stay conservative"
        );
        assert!(without.validate().is_ok());

        let with: SemanticProviderDescriptor = serde_json::from_str(
            r#"{
                "id": "new-provider",
                "version": 1,
                "schema_version": 1,
                "capabilities": {
                    "ops": 2,
                    "compose_delta": false,
                    "constrained_edit": false
                },
                "tier": 2,
                "evidence": {
                    "freshness": "current",
                    "determinism": "deterministic",
                    "revision_binding": "exact",
                    "provenance": { "scope": "direct", "uncertainty": "none" },
                    "corroboration": { "measured": { "independent_sources": 2 } }
                }
            }"#,
        )
        .expect("a descriptor with a tier and evidence must decode");
        assert_eq!(with.tier, Some(2));
        assert_eq!(with.intelligence_tier(), Some(IntelligenceTier::COMPILER));
        assert_eq!(
            with.evidence,
            EvidenceDimensions {
                freshness: EvidenceFreshness::Current,
                determinism: EvidenceDeterminism::Deterministic,
                revision_binding: RevisionBinding::Exact,
                provenance: EvidenceProvenance {
                    scope: EvidenceScope::Direct,
                    uncertainty: EvidenceUncertainty::None,
                },
                corroboration: EvidenceCorroboration::Measured {
                    independent_sources: 2,
                },
            }
        );
        assert!(with.validate().is_ok());

        // A partial evidence block decodes with the absent axes conservative.
        let partial: SemanticProviderDescriptor = serde_json::from_str(
            r#"{
                "id": "partial-provider",
                "version": 1,
                "schema_version": 1,
                "capabilities": {
                    "ops": 2,
                    "compose_delta": false,
                    "constrained_edit": false
                },
                "evidence": { "determinism": "deterministic" }
            }"#,
        )
        .expect("a partial evidence block decodes additively");
        assert_eq!(
            partial.evidence.determinism,
            EvidenceDeterminism::Deterministic
        );
        assert_eq!(partial.evidence.freshness, EvidenceFreshness::Unknown);
        assert_eq!(partial.evidence.revision_binding, RevisionBinding::Unknown);
        assert_eq!(partial.evidence.provenance.scope, EvidenceScope::Unknown);
        assert_eq!(
            partial.evidence.provenance.uncertainty,
            EvidenceUncertainty::Unknown
        );
        assert_eq!(
            partial.evidence.corroboration,
            EvidenceCorroboration::Unknown
        );
        assert!(!partial.evidence.is_undeclared());

        // A hostile evidence level is refused at decode, never coerced into
        // a trusted axis.
        assert!(
            serde_json::from_str::<SemanticProviderDescriptor>(
                r#"{
                    "id": "hostile-evidence",
                    "version": 1,
                    "schema_version": 1,
                    "capabilities": {
                        "ops": 2,
                        "compose_delta": false,
                        "constrained_edit": false
                    },
                    "evidence": { "freshness": "future" }
                }"#,
            )
            .is_err(),
            "an unknown freshness level must be refused, not coerced"
        );

        // An out-of-range tier still DECODES (the wire stays additive), but
        // never validates: the handshake refuses it typed.
        let hostile: SemanticProviderDescriptor = serde_json::from_str(
            r#"{
                "id": "hostile-provider",
                "version": 1,
                "schema_version": 1,
                "capabilities": {
                    "ops": 2,
                    "compose_delta": false,
                    "constrained_edit": false
                },
                "tier": 9
            }"#,
        )
        .expect("hostile tiers decode and are refused at validation");
        assert!(matches!(
            hostile.validate(),
            Err(SemanticError::InvalidIntelligenceTier { tier: 9 })
        ));
        assert_eq!(hostile.intelligence_tier(), None);

        // Round-trip of the newtype is transparent (plain integer) and
        // `None` serializes to explicit null.
        assert_eq!(
            serde_json::to_string(&IntelligenceTier::COMPILER).unwrap(),
            "2"
        );
        assert_eq!(
            serde_json::to_string(&with.intelligence_tier()).unwrap(),
            "2"
        );
        assert_eq!(
            serde_json::to_string(&without.intelligence_tier()).unwrap(),
            "null"
        );
    }
}
