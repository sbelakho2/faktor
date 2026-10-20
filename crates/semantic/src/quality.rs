//! @authority-collapse: authority-bearing
//!
//! The ONE authoritative quality gate (per-operation fidelity, completeness,
//! tier and evidence) plus the typed selection outcome.
//!
//! Authoritative consumers (merge/review, planning context, affected-set
//! risk, verification metadata) must never inspect provider-wide scalars or
//! hand-roll their own fidelity checks. They build a
//! [`SemanticQualityRequirement`], ask
//! [`crate::registry::SemanticProviderRegistry::select_quality`], and accept
//! the typed result or the typed refusal. Selection is capability-driven
//! first and quality-driven second, so a provider that advertises an
//! operation but declares low (or no) quality can never satisfy an
//! obligation that requires more.
//!
//! Performance: the per-operation quality lookup is one field access
//! (`O(1)`); selection scans the registered providers once, in registration
//! order, and performs constant-time comparisons per candidate.

use crate::types::{
    SemanticCapabilities, SemanticCompleteness, SemanticError, SemanticEvidenceSource,
    SemanticFidelity, SemanticOp, SemanticOperationQuality, SemanticProvider,
    SemanticProviderDescriptor,
};

/// The quality a caller REQUIRES for one operation. Capabilities are part of
/// the requirement because an operation is only selectable when its
/// advertisement covers the caller's needs (for example a delta caller may
/// require `compose_delta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SemanticQualityRequirement {
    pub op: SemanticOp,
    pub capabilities: SemanticCapabilities,
    pub fidelity: SemanticFidelity,
    pub completeness: SemanticCompleteness,
    /// Optional tier floor. `None` means the caller does not require a
    /// particular capability tier; a declared floor must be met by a
    /// DECLARED tier (silence is never assumed high).
    pub minimum_tier: Option<u8>,
}

impl SemanticQualityRequirement {
    pub const fn new(
        op: SemanticOp,
        capabilities: SemanticCapabilities,
        fidelity: SemanticFidelity,
        completeness: SemanticCompleteness,
    ) -> Self {
        Self {
            op,
            capabilities,
            fidelity,
            completeness,
            minimum_tier: None,
        }
    }

    /// The documented authoritative table: context/explain may be
    /// heuristic/partial, affected requires parsed structure and partial
    /// coverage, snapshots/deltas need compiler-exact conservative-complete
    /// evidence, and verification demands proof.
    pub const fn authoritative(op: SemanticOp) -> Self {
        Self {
            op,
            capabilities: SemanticCapabilities::for_op(op),
            fidelity: op.minimum_fidelity(),
            completeness: op.minimum_completeness(),
            minimum_tier: None,
        }
    }

    /// A consult that requires DECLARED trust but no particular coverage:
    /// fidelity must be at least the operation's minimum and completeness
    /// may stay `Unknown`. Used by advisory planners that must not consume
    /// an undeclared provider but can tolerate partial answers.
    pub const fn declared(op: SemanticOp) -> Self {
        Self {
            op,
            capabilities: SemanticCapabilities::for_op(op),
            fidelity: op.minimum_fidelity(),
            completeness: SemanticCompleteness::Unknown,
            minimum_tier: None,
        }
    }

    /// No quality floor: capability coverage alone decides. This is the
    /// non-authoritative consult path (best-effort observability), not a
    /// trust widening for authoritative callers.
    pub const fn advisory(op: SemanticOp) -> Self {
        Self {
            op,
            capabilities: SemanticCapabilities::for_op(op),
            fidelity: SemanticFidelity::None,
            completeness: SemanticCompleteness::Unknown,
            minimum_tier: None,
        }
    }

    pub const fn with_capabilities(mut self, capabilities: SemanticCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub const fn with_compose_delta(mut self, enabled: bool) -> Self {
        self.capabilities = self.capabilities.with_compose_delta(enabled);
        self
    }

    pub const fn with_minimum_tier(mut self, tier: u8) -> Self {
        self.minimum_tier = Some(tier);
        self
    }

    /// The single acceptance rule. Ordered enums make these comparisons
    /// total; an undeclared fidelity is [`SemanticFidelity::None`] and can
    /// never pass a real floor. An undeclared tier cannot satisfy a tier
    /// floor.
    pub fn accepts(&self, quality: &SemanticOperationQuality) -> bool {
        if quality.fidelity < self.fidelity || quality.completeness < self.completeness {
            return false;
        }
        match self.minimum_tier {
            None => true,
            Some(minimum) => quality
                .tier
                .and_then(|raw| crate::intelligence::IntelligenceTier::parse(raw).ok())
                .is_some_and(|declared| declared.raw() >= minimum),
        }
    }

    /// Build the typed rejection for one candidate that covered the
    /// operation but failed the quality floor.
    pub fn rejection(
        &self,
        source: impl Into<String>,
        quality: &SemanticOperationQuality,
    ) -> SemanticError {
        SemanticError::QualityBelowRequirement {
            op: self.op.as_str().to_string(),
            source: source.into(),
            required_fidelity: self.fidelity,
            declared_fidelity: quality.fidelity,
            required_completeness: self.completeness,
            declared_completeness: quality.completeness,
        }
    }

    /// Build the typed rejection when no candidate covered the requirement
    /// at all.
    pub fn unavailable(&self, detail: impl Into<String>) -> SemanticError {
        SemanticError::QualityUnavailable {
            op: self.op.as_str().to_string(),
            detail: detail.into(),
        }
    }
}

/// The accepted candidate from the central quality gate.
pub enum SemanticQualitySelection<'a> {
    Provider {
        provider: &'a dyn SemanticProvider,
        quality: SemanticOperationQuality,
    },
    Fallback {
        quality: SemanticOperationQuality,
    },
}

impl std::fmt::Debug for SemanticQualitySelection<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider { provider, quality } => f
                .debug_struct("Provider")
                .field("provider", &provider.id().as_str())
                .field("quality", quality)
                .finish(),
            Self::Fallback { quality } => f
                .debug_struct("Fallback")
                .field("quality", quality)
                .finish(),
        }
    }
}

/// The per-operation quality of one provider. Explicit per-operation
/// declarations win; otherwise the provider's legacy scalar declaration is
/// used (honouring overrides of [`SemanticProvider::fidelity`] /
/// [`SemanticProvider::completeness`]). O(1).
pub fn effective_quality(
    provider: &dyn SemanticProvider,
    descriptor: &SemanticProviderDescriptor,
    op: SemanticOp,
) -> SemanticOperationQuality {
    if let Some(quality) = descriptor.operation_quality(op) {
        return quality.clone();
    }
    SemanticOperationQuality {
        fidelity: provider.fidelity(),
        completeness: provider.completeness(),
        tier: descriptor.tier,
        evidence: descriptor.evidence,
        provenance: SemanticEvidenceSource::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intelligence::EvidenceDimensions;

    fn quality(
        fidelity: SemanticFidelity,
        completeness: SemanticCompleteness,
    ) -> SemanticOperationQuality {
        SemanticOperationQuality::new(
            fidelity,
            completeness,
            None,
            EvidenceDimensions::UNKNOWN,
            SemanticEvidenceSource::Unknown,
        )
    }

    #[test]
    fn authoritative_requirements_follow_the_documented_table() {
        let context = SemanticQualityRequirement::authoritative(SemanticOp::Context);
        assert_eq!(context.fidelity, SemanticFidelity::Heuristic);
        assert_eq!(context.completeness, SemanticCompleteness::Partial);
        let affected = SemanticQualityRequirement::authoritative(SemanticOp::Affected);
        assert_eq!(affected.fidelity, SemanticFidelity::Structural);
        assert_eq!(affected.completeness, SemanticCompleteness::Partial);
        let delta = SemanticQualityRequirement::authoritative(SemanticOp::Delta);
        assert_eq!(delta.fidelity, SemanticFidelity::CompilerExact);
        assert_eq!(
            delta.completeness,
            SemanticCompleteness::ConservativeComplete
        );
        let verify = SemanticQualityRequirement::authoritative(SemanticOp::Verify);
        assert_eq!(verify.fidelity, SemanticFidelity::Proof);
        assert_eq!(
            verify.completeness,
            SemanticCompleteness::ConservativeComplete
        );
    }

    #[test]
    fn low_fidelity_never_satisfies_a_high_fidelity_requirement() {
        let requirement = SemanticQualityRequirement::authoritative(SemanticOp::Delta);
        assert!(!requirement.accepts(&quality(
            SemanticFidelity::Structural,
            SemanticCompleteness::ConservativeComplete,
        )));
        assert!(!requirement.accepts(&quality(
            SemanticFidelity::CompilerExact,
            SemanticCompleteness::Partial,
        )));
        assert!(requirement.accepts(&quality(
            SemanticFidelity::CompilerExact,
            SemanticCompleteness::ConservativeComplete,
        )));
        assert!(requirement.accepts(&quality(
            SemanticFidelity::Proof,
            SemanticCompleteness::Complete,
        )));
    }

    #[test]
    fn undeclared_quality_is_untrusted() {
        let requirement = SemanticQualityRequirement::authoritative(SemanticOp::Context);
        assert!(!requirement.accepts(&quality(
            SemanticFidelity::None,
            SemanticCompleteness::Unknown,
        )));
        assert!(requirement.accepts(&quality(
            SemanticFidelity::Heuristic,
            SemanticCompleteness::Partial,
        )));
    }

    #[test]
    fn tier_floor_requires_a_declared_tier() {
        let requirement =
            SemanticQualityRequirement::authoritative(SemanticOp::Context).with_minimum_tier(2);
        let mut undeclared = quality(
            SemanticFidelity::CompilerExact,
            SemanticCompleteness::Complete,
        );
        assert!(!requirement.accepts(&undeclared));
        undeclared.tier = Some(2);
        assert!(requirement.accepts(&undeclared));
        undeclared.tier = Some(1);
        assert!(!requirement.accepts(&undeclared));
    }

    #[test]
    fn provenance_tiers_are_the_documented_ladder() {
        assert_eq!(SemanticEvidenceSource::Filesystem.tier_raw(), Some(0));
        assert_eq!(SemanticEvidenceSource::TreeSitter.tier_raw(), Some(1));
        assert_eq!(SemanticEvidenceSource::Lsp.tier_raw(), Some(2));
        assert_eq!(SemanticEvidenceSource::Graph.tier_raw(), Some(3));
        assert_eq!(SemanticEvidenceSource::RuntimeHistory.tier_raw(), Some(4));
        assert_eq!(SemanticEvidenceSource::External.tier_raw(), Some(5));
        assert_eq!(SemanticEvidenceSource::Unknown.tier_raw(), None);
    }
}
