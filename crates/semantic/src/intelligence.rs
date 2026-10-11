//! @authority-collapse: authority-bearing
//!
//! Intelligence-tier model (`docs/specs/autonomous-engineering.md`).
//!
//! Faktor is STANDALONE: it always knows which intelligence tier is
//! available locally and adjusts confidence honestly. No tier, capability or
//! confidence value here ever depends on Tangerine or any other external
//! provider — an external provider is at most ONE optional source of a
//! declared tier, and its absence simply leaves the native Tier 0 truth in
//! place.
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
//! Honesty rules enforced here:
//!
//! - The generic fallback is Tier 0 for context/delta/affected/explain
//!   (bounded searches plus heuristics); it never claims a tier or a
//!   capability it cannot deliver, and unavailable operations still report
//!   their true tier with zero confidence.
//! - A registered provider's tier comes ONLY from the optional
//!   [`crate::types::SemanticProviderDescriptor::tier`] field, validated to
//!   `0..=5` at handshake. An absent declaration is `None` (unknown):
//!   confidence is then computed WITHOUT any tier bonus, never assumed high.
//! - Tier alone never creates confidence: an undeclared fidelity pins the
//!   result to zero, so a high tier can never launder an evidence-quality
//!   claim that was not made.

use serde::{Deserialize, Serialize};

use crate::fallback::GENERIC_FALLBACK_ID;
use crate::types::{
    SemanticCompleteness, SemanticError, SemanticFidelity, SemanticOp, SemanticOperationStatus,
};

/// The highest valid intelligence tier (tiers are `0..=5`).
pub const MAX_INTELLIGENCE_TIER: u8 = 5;

/// A validated intelligence tier (`0..=5`). Out-of-range values are a typed
/// refusal at construction, so an impossible tier can never reach a status
/// surface or a confidence computation.
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

/// The confidence mapping in basis points (`0..=10_000`).
///
/// ```text
/// confidence = clamp(fidelity_base × completeness_factor / 10_000 + tier_bonus, 0, 10_000)
/// ```
///
/// with an undeclared fidelity ([`SemanticFidelity::None`]) pinning the
/// result to `0` — a tier can never create an evidence claim that was not
/// made.
///
/// Fidelity base (bps):
///
/// | fidelity                | base |
/// |-------------------------|------|
/// | `None`                  |    0 |
/// | `Heuristic`             | 2500 |
/// | `Structural`            | 5000 |
/// | `CompilerExact`         | 8000 |
/// | `Proof`                 | 9500 |
///
/// Completeness factor (bps, integer division):
///
/// | completeness              | factor |
/// |---------------------------|--------|
/// | `Unknown`                 |   5000 |
/// | `Partial`                 |   7500 |
/// | `ConservativeComplete`    |   9000 |
/// | `Complete`                |  10000 |
///
/// Tier bonus (bps, additive; declared tiers only — absent declarations get
/// NO bonus, never an assumed-high one):
///
/// | tier     | bonus |
/// |----------|-------|
/// | absent   |     0 |
/// | 0        |     0 |
/// | 1        |   200 |
/// | 2        |   400 |
/// | 3        |   600 |
/// | 4        |   800 |
/// | 5        |  1000 |
///
/// The mapping is monotone in each axis: higher fidelity, more complete
/// coverage and a higher declared tier never lower the result.
pub fn confidence_bps_for(
    fidelity: SemanticFidelity,
    completeness: SemanticCompleteness,
    tier: Option<IntelligenceTier>,
) -> u16 {
    if fidelity == SemanticFidelity::None {
        // No declared evidence quality: confidence stays 0 regardless of
        // completeness or tier.
        return 0;
    }
    let base: u32 = match fidelity {
        SemanticFidelity::None => 0,
        SemanticFidelity::Heuristic => 2_500,
        SemanticFidelity::Structural => 5_000,
        SemanticFidelity::CompilerExact => 8_000,
        SemanticFidelity::Proof => 9_500,
    };
    let factor: u32 = match completeness {
        SemanticCompleteness::Unknown => 5_000,
        SemanticCompleteness::Partial => 7_500,
        SemanticCompleteness::ConservativeComplete => 9_000,
        SemanticCompleteness::Complete => 10_000,
    };
    let bonus = tier.map_or(0, |tier| u32::from(tier.raw()) * 200);
    (base * factor / 10_000).saturating_add(bonus).min(10_000) as u16
}

/// The typed per-operation capability projection: [`IntelligenceCapability`]
/// reuses/absorbs [`SemanticOperationStatus`] (existing consumers keep the
/// old struct) and adds the tier plus the derived bounded confidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntelligenceCapability {
    pub op: SemanticOp,
    /// The declared tier, or `None` when no tier was declared (unknown,
    /// never assumed high).
    pub tier: Option<IntelligenceTier>,
    pub available: bool,
    pub fidelity: SemanticFidelity,
    pub completeness: SemanticCompleteness,
    pub source: String,
}

impl IntelligenceCapability {
    /// Project an existing [`SemanticOperationStatus`] plus a tier into the
    /// intelligence surface, without changing the status type its consumers
    /// already use.
    pub fn from_status(
        op: SemanticOp,
        status: SemanticOperationStatus,
        tier: Option<IntelligenceTier>,
    ) -> Self {
        Self {
            op,
            tier,
            available: status.available,
            fidelity: status.fidelity,
            completeness: status.completeness,
            source: status.source,
        }
    }

    /// The bounded confidence (`0..=10_000`) for this capability. An
    /// unavailable operation always reports `0`: a capability that is not
    /// served cannot carry confidence.
    pub fn confidence_bps(&self) -> u16 {
        if !self.available {
            return 0;
        }
        confidence_bps_for(self.fidelity, self.completeness, self.tier)
    }
}

/// The generic fallback's honest per-operation intelligence capability: Tier
/// 0 (filesystem + text search), heuristic fidelity, its real completeness
/// and source, exactly as [`crate::registry::fallback_operation_status`]
/// reports — with the tier made explicit instead of implied.
pub fn fallback_intelligence_capability(op: SemanticOp) -> IntelligenceCapability {
    IntelligenceCapability::from_status(
        op,
        crate::registry::fallback_operation_status(op),
        Some(IntelligenceTier::FILESYSTEM),
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
}

impl NativeIntelligenceService {
    /// The bounded confidence for this service under the shared mapping.
    pub fn confidence_bps(&self) -> u16 {
        if !self.available {
            return 0;
        }
        confidence_bps_for(self.fidelity, self.completeness, Some(self.tier))
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
        },
        NativeIntelligenceService {
            name: "text_search".to_string(),
            tier: IntelligenceTier::FILESYSTEM,
            available: true,
            fidelity: SemanticFidelity::Heuristic,
            completeness: SemanticCompleteness::Partial,
        },
        NativeIntelligenceService {
            name: "read_only_pass".to_string(),
            tier: IntelligenceTier::FILESYSTEM,
            available: true,
            fidelity: SemanticFidelity::Heuristic,
            completeness: SemanticCompleteness::Partial,
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
    fn confidence_mapping_table_is_exact_and_monotone() {
        use SemanticCompleteness as C;
        use SemanticFidelity as F;
        let t0 = Some(IntelligenceTier::FILESYSTEM);
        let t2 = Some(IntelligenceTier::COMPILER);
        let t5 = Some(IntelligenceTier::ENRICHED);

        // Exact table values.
        assert_eq!(confidence_bps_for(F::None, C::Complete, t5), 0);
        assert_eq!(confidence_bps_for(F::None, C::Unknown, None), 0);
        assert_eq!(confidence_bps_for(F::Heuristic, C::Unknown, t0), 1_250);
        assert_eq!(confidence_bps_for(F::Heuristic, C::Partial, t0), 1_875);
        assert_eq!(
            confidence_bps_for(F::Heuristic, C::ConservativeComplete, None),
            2_250
        );
        assert_eq!(confidence_bps_for(F::Heuristic, C::Complete, t5), 3_500);
        assert_eq!(
            confidence_bps_for(F::Structural, C::ConservativeComplete, t2),
            4_900
        );
        assert_eq!(
            confidence_bps_for(F::CompilerExact, C::ConservativeComplete, t2),
            7_600
        );
        assert_eq!(confidence_bps_for(F::Proof, C::Complete, None), 9_500);
        assert_eq!(
            confidence_bps_for(F::Proof, C::Complete, t5),
            10_000,
            "9500 + 1000 bonus clamps to the 10000 bound"
        );

        // Bounded for every combination.
        let fidelities = [
            F::None,
            F::Heuristic,
            F::Structural,
            F::CompilerExact,
            F::Proof,
        ];
        let completenesses = [C::Unknown, C::Partial, C::ConservativeComplete, C::Complete];
        let tiers = [
            None,
            Some(IntelligenceTier::FILESYSTEM),
            Some(IntelligenceTier::SYNTAX),
            Some(IntelligenceTier::COMPILER),
            Some(IntelligenceTier::GRAPH),
            Some(IntelligenceTier::RUNTIME),
            Some(IntelligenceTier::ENRICHED),
        ];
        for fidelity in fidelities {
            for completeness in completenesses {
                for tier in tiers {
                    let value = confidence_bps_for(fidelity, completeness, tier);
                    assert!(value <= 10_000, "{fidelity:?}/{completeness:?}/{tier:?}");
                }
            }
        }

        // Monotone in fidelity (fixed completeness/tier).
        for completeness in completenesses {
            for tier in tiers {
                for pair in fidelities.windows(2) {
                    assert!(
                        confidence_bps_for(pair[0], completeness, tier)
                            <= confidence_bps_for(pair[1], completeness, tier),
                        "fidelity {pair:?} must not lower confidence"
                    );
                }
            }
        }
        // Monotone in completeness (fixed fidelity/tier).
        for fidelity in fidelities {
            for tier in tiers {
                for pair in completenesses.windows(2) {
                    assert!(
                        confidence_bps_for(fidelity, pair[0], tier)
                            <= confidence_bps_for(fidelity, pair[1], tier),
                        "completeness {pair:?} must not lower confidence"
                    );
                }
            }
        }
        // Monotone in tier, with absent = no bonus: None == tier 0, and
        // every declared step is non-decreasing (strictly up to tier 5).
        for fidelity in fidelities {
            for completeness in completenesses {
                assert_eq!(
                    confidence_bps_for(fidelity, completeness, None),
                    confidence_bps_for(fidelity, completeness, t0),
                    "an absent tier adds NO bonus"
                );
                for pair in tiers.windows(2) {
                    assert!(
                        confidence_bps_for(fidelity, completeness, pair[0])
                            <= confidence_bps_for(fidelity, completeness, pair[1]),
                        "tier {pair:?} must not lower confidence"
                    );
                }
            }
        }
        assert!(
            confidence_bps_for(F::CompilerExact, C::Complete, None)
                < confidence_bps_for(F::CompilerExact, C::Complete, t5),
            "an undeclared tier must report LESS than a declared tier 5"
        );
    }

    #[test]
    fn fallback_reports_tier_zero_and_heuristic_fidelity() {
        for op in [
            SemanticOp::Context,
            SemanticOp::Delta,
            SemanticOp::Affected,
            SemanticOp::Explain,
        ] {
            let capability = fallback_intelligence_capability(op);
            assert!(capability.available, "{op:?}");
            assert_eq!(capability.tier, Some(IntelligenceTier::FILESYSTEM));
            assert_eq!(capability.fidelity, SemanticFidelity::Heuristic);
            assert_eq!(capability.source, GENERIC_FALLBACK_ID);
            assert!(capability.confidence_bps() > 0, "{op:?}");
        }
        // The fallback never claims what it cannot deliver; even the
        // unavailable operations report their true tier with zero
        // confidence.
        for op in [SemanticOp::Snapshot, SemanticOp::Verify] {
            let capability = fallback_intelligence_capability(op);
            assert!(!capability.available, "{op:?}");
            assert_eq!(capability.tier, Some(IntelligenceTier::FILESYSTEM));
            assert_eq!(capability.fidelity, SemanticFidelity::Heuristic);
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
            assert!(service.confidence_bps() <= 10_000);
        }
        assert_eq!(report.operations.len(), SemanticOp::ALL.len());
        assert!(report
            .operations
            .iter()
            .all(|capability| capability.tier == Some(IntelligenceTier::FILESYSTEM)));
    }

    #[test]
    fn descriptor_tier_is_additively_optional_on_the_wire() {
        // A descriptor encoded by an OLD provider (no tier field) decodes
        // with `None`, and a descriptor with `tier` decodes with it set.
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
                "tier": 2
            }"#,
        )
        .expect("a descriptor with a tier must decode");
        assert_eq!(with.tier, Some(2));
        assert_eq!(with.intelligence_tier(), Some(IntelligenceTier::COMPILER));
        assert!(with.validate().is_ok());

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
