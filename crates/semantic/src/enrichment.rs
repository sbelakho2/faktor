//! @authority-collapse: authority-bearing
//!
//! System-intelligence contract enrichment over the semantic-provider
//! registry: the production adapter for the neutral
//! [`faktor_core::contract_compile::SystemIntelligenceProvider`] seam.
//!
//! The adapter is strictly ADDITIVE and honest by construction:
//!
//! - It reports ONLY what a registered provider has actually validated (a
//!   handshake-validated descriptor: its covered operations and declared
//!   tier / fidelity / completeness / evidence dimensions). A provider whose
//!   descriptor is not validated yet is invisible here, exactly as it is to
//!   selection and dispatch, and a fallback-only registry yields an EMPTY
//!   enrichment — the native contract stays byte-identical.
//! - Every statement is a bounded DISCLOSURE risk carrying the confidence
//!   the shared intelligence mapping derives from the declared dimensions,
//!   with `external:semantic` provenance attached by the compiler. It never
//!   fabricates constraints, invariants or proof obligations on the
//!   provider's behalf, and its findings are data: native evidence still
//!   discharges the contract's claims.
//! - [`SemanticIntelligenceAdapter::enrich`] is total: it never errors and
//!   never fails a turn; a missing, unvalidated or refusing provider simply
//!   contributes nothing.

use std::sync::Arc;

use faktor_core::contract::RiskSeverity;
use faktor_core::contract_compile::{
    EnrichmentKind, EnrichmentRequest, EnrichmentStatement, SystemIntelligenceProvider,
    MAX_STATEMENT_BYTES,
};

use crate::intelligence::{confidence_bps_for, ConfidenceDimensions};
use crate::registry::SemanticProviderRegistry;
use crate::types::{SemanticCapabilities, SemanticCompleteness, SemanticFidelity, SemanticOp};

/// Stable provider id of the registry-level adapter: every enriched
/// statement carries `external:semantic` provenance, and the BASIS names the
/// concrete semantic provider the disclosure came from.
pub const SEMANTIC_ENRICHMENT_PROVIDER_ID: &str = "semantic";

/// The production bridge from the daemon's semantic-provider registry to the
/// contract compiler's neutral enrichment seam. Construct it over the SAME
/// `Arc<SemanticProviderRegistry>` the agent and server introspect (one
/// authority), so the disclosure can never describe a registry the daemon is
/// not actually serving.
pub struct SemanticIntelligenceAdapter {
    registry: Arc<SemanticProviderRegistry>,
}

impl SemanticIntelligenceAdapter {
    pub fn new(registry: Arc<SemanticProviderRegistry>) -> Self {
        Self { registry }
    }

    /// The registry this adapter reads (the daemon's ONE authority).
    pub fn registry(&self) -> &Arc<SemanticProviderRegistry> {
        &self.registry
    }
}

impl SystemIntelligenceProvider for SemanticIntelligenceAdapter {
    fn provider_id(&self) -> &str {
        SEMANTIC_ENRICHMENT_PROVIDER_ID
    }

    /// One bounded disclosure per (validated provider, covered operation):
    /// the declared intelligence surface, its evidence dimensions and the
    /// confidence the shared mapping derives — never a fabricated claim.
    fn enrich(&self, request: &EnrichmentRequest<'_>) -> Result<Vec<EnrichmentStatement>, String> {
        let mut statements = Vec::new();
        for provider in self.registry.providers() {
            let Some(descriptor) = provider.validated_descriptor() else {
                // Unvalidated (e.g. an external provider before its
                // handshake): invisible, exactly like selection.
                continue;
            };
            for op in SemanticOp::ALL {
                if statements.len() >= request.max_statements {
                    return Ok(statements);
                }
                if !descriptor.covers_validated(SemanticCapabilities::for_op(op)) {
                    continue;
                }
                let dimensions = ConfidenceDimensions::new(
                    descriptor.fidelity.unwrap_or(SemanticFidelity::None),
                    descriptor
                        .completeness
                        .unwrap_or(SemanticCompleteness::Unknown),
                    descriptor.evidence,
                );
                let confidence_bps = confidence_bps_for(&dimensions);
                if confidence_bps == 0 {
                    // An undeclared fidelity pins confidence to zero: there
                    // is no evidence claim to disclose.
                    continue;
                }
                let tier = descriptor
                    .intelligence_tier()
                    .map_or_else(|| "undeclared".to_string(), |tier| tier.raw().to_string());
                let text = bounded(
                    &format!(
                        "external semantic provider `{}` declares {} intelligence (tier {tier}, fidelity {:?}, completeness {:?}) at confidence {confidence_bps} bps; its findings are data only and native evidence still discharges this contract's claims",
                        provider.id(),
                        op.as_str(),
                        dimensions.fidelity,
                        dimensions.completeness,
                    ),
                    MAX_STATEMENT_BYTES,
                );
                let basis = bounded(
                    &format!(
                        "handshake-validated semantic provider descriptor from `{}` covering {}",
                        provider.id(),
                        op.as_str()
                    ),
                    MAX_STATEMENT_BYTES,
                );
                statements.push(EnrichmentStatement {
                    kind: EnrichmentKind::Risk,
                    text,
                    confidence_bps,
                    basis,
                    evidence: None,
                    binding: None,
                    severity: Some(RiskSeverity::Low),
                    advisory: false,
                });
            }
        }
        Ok(statements)
    }
}

/// Truncate `text` to at most `max` bytes on a character boundary (the
/// compiler independently enforces the bound; this keeps the adapter from
/// ever proposing a statement the compiler must skip).
fn bounded(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fallback::GenericSemanticFallback;
    use crate::test_support::provider_id;
    use crate::types::{SemanticProvider, SemanticProviderDescriptor, SEMANTIC_SCHEMA_VERSION};
    use faktor_core::contract::EngineeringContract;
    use faktor_core::contract_compile::{
        compile_contract, compile_contract_with, ContractCompileRequest, RepositoryFile,
        RepositorySnapshot,
    };
    use faktor_core::ContractOrigin;

    /// An in-process provider whose ONLY behaviour is a declared descriptor:
    /// exactly the shape a visible semantic service presents to the
    /// registry.
    struct DeclaredProvider {
        id: crate::types::SemanticProviderId,
        capabilities: SemanticCapabilities,
        fidelity: Option<SemanticFidelity>,
        completeness: Option<SemanticCompleteness>,
        tier: Option<u8>,
    }

    impl DeclaredProvider {
        fn covering_affected() -> Self {
            Self {
                id: provider_id("declared-affects"),
                capabilities: SemanticCapabilities::AFFECTED,
                fidelity: Some(SemanticFidelity::Structural),
                completeness: Some(SemanticCompleteness::ConservativeComplete),
                tier: Some(2),
            }
        }
    }

    impl SemanticProvider for DeclaredProvider {
        fn id(&self) -> crate::types::SemanticProviderId {
            self.id.clone()
        }

        fn version(&self) -> u32 {
            1
        }

        fn capabilities(&self) -> SemanticCapabilities {
            self.capabilities
        }

        fn descriptor(&self) -> SemanticProviderDescriptor {
            let mut descriptor = SemanticProviderDescriptor::new(
                self.id.clone(),
                self.version(),
                SEMANTIC_SCHEMA_VERSION,
                self.capabilities,
            );
            descriptor.fidelity = self.fidelity;
            descriptor.completeness = self.completeness;
            descriptor.tier = self.tier;
            descriptor
        }
    }

    fn registry_with(provider: Option<DeclaredProvider>) -> Arc<SemanticProviderRegistry> {
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        if let Some(provider) = provider {
            registry.register(Arc::new(provider));
        }
        Arc::new(registry)
    }

    fn enrich(registry: &Arc<SemanticProviderRegistry>) -> Vec<EnrichmentStatement> {
        let contract = EngineeringContract::default();
        let adapter = SemanticIntelligenceAdapter::new(registry.clone());
        adapter
            .enrich(&EnrichmentRequest {
                goal: "fix the refresh token race",
                intent_tokens: &[],
                build_systems: &["cargo".to_string()],
                repository_digest: "digest",
                native_contract: &contract,
                max_statements: 24,
            })
            .expect("the adapter is total")
    }

    #[test]
    fn fallback_only_registry_yields_no_enrichment() {
        let registry = registry_with(None);
        assert!(enrich(&registry).is_empty());
        // The adapter is also honest about its provenance id.
        let adapter = SemanticIntelligenceAdapter::new(registry);
        assert_eq!(adapter.provider_id(), "semantic");
    }

    #[test]
    fn validated_provider_yields_bounded_disclosure_with_declared_dimensions() {
        let registry = registry_with(Some(DeclaredProvider::covering_affected()));
        let statements = enrich(&registry);
        assert_eq!(
            statements.len(),
            1,
            "only the ONE covered op is disclosed: {statements:?}"
        );
        let statement = &statements[0];
        assert_eq!(statement.kind, EnrichmentKind::Risk);
        assert!(statement.text.contains("declared-affects"));
        assert!(statement.text.contains("affected"));
        assert!(
            statement.text.contains("native evidence still discharges"),
            "{}",
            statement.text
        );
        assert!(statement.text.len() <= MAX_STATEMENT_BYTES);
        assert!(statement.basis.contains("declared-affects"));
        assert_eq!(
            statement.severity,
            Some(RiskSeverity::Low),
            "a capability disclosure is never escalated"
        );
        // The confidence is exactly what the shared intelligence mapping
        // derives from the DECLARED dimensions (absent evidence axes stay
        // conservative); the tier never launders confidence.
        let expected = confidence_bps_for(&ConfidenceDimensions::new(
            SemanticFidelity::Structural,
            SemanticCompleteness::ConservativeComplete,
            Default::default(),
        ));
        assert!(expected > 0);
        assert_eq!(statement.confidence_bps, expected);
    }

    #[test]
    fn unvalidated_provider_is_invisible() {
        // An out-of-range tier fails descriptor validation: the provider is
        // not selectable and never enriches.
        let mut provider = DeclaredProvider::covering_affected();
        provider.tier = Some(9);
        let registry = registry_with(Some(provider));
        assert!(enrich(&registry).is_empty());
    }

    #[test]
    fn undeclared_fidelity_pins_confidence_to_zero_and_discloses_nothing() {
        let mut provider = DeclaredProvider::covering_affected();
        provider.fidelity = None;
        let registry = registry_with(Some(provider));
        assert!(enrich(&registry).is_empty());
    }

    #[test]
    fn statement_cap_is_respected() {
        let registry = registry_with(Some(DeclaredProvider::covering_affected()));
        let contract = EngineeringContract::default();
        let adapter = SemanticIntelligenceAdapter::new(registry);
        let statements = adapter
            .enrich(&EnrichmentRequest {
                goal: "g",
                intent_tokens: &[],
                build_systems: &[],
                repository_digest: "d",
                native_contract: &contract,
                max_statements: 0,
            })
            .unwrap();
        assert!(statements.is_empty(), "zero cap yields nothing");
    }

    #[test]
    fn compilation_with_the_adapter_is_additive_and_native_statements_survive() {
        let snapshot = RepositorySnapshot::from_files(vec![
            RepositoryFile::with_content("Cargo.toml", "[package]\nname = \"auth\"\n"),
            RepositoryFile::with_content("src/auth/refresh.rs", "pub fn refresh() -> u64 { 0 }\n"),
            RepositoryFile::with_content(
                "tests/refresh_tokens.rs",
                "#[test]\nfn replayed_refresh_token_fails() {}\n",
            ),
        ]);
        let request = ContractCompileRequest {
            goal: "fix the refresh token race".to_string(),
            repository: snapshot,
        };
        let native = compile_contract(&request);
        let registry = registry_with(Some(DeclaredProvider::covering_affected()));
        let adapter = SemanticIntelligenceAdapter::new(registry);
        let enriched = compile_contract_with(&request, Some(&adapter));
        // The native statements are byte-identical: every native-origin
        // statement survives unchanged and no native statement was rewritten.
        for obligation in &native.contract.proof_obligations {
            assert!(enriched
                .contract
                .proof_obligations
                .iter()
                .any(|o| o == obligation));
        }
        for risk in &native.contract.risks {
            assert!(enriched.contract.risks.iter().any(|r| r == risk));
        }
        // The added statement carries external:semantic provenance, and
        // every external statement is one this adapter proposed.
        let external: Vec<&faktor_core::contract::RiskHypothesis> = enriched
            .contract
            .risks
            .iter()
            .filter(|risk| {
                matches!(
                    risk.origin,
                    ContractOrigin::External { ref provider } if provider == "semantic"
                )
            })
            .collect();
        assert!(!external.is_empty());
        assert!(external.iter().all(|risk| risk
            .description
            .contains("native evidence still discharges")));
        assert_eq!(enriched.enrichments.len(), 1);
        assert_eq!(enriched.enrichments[0].provider, "semantic");
        assert_eq!(enriched.enrichments[0].applied, external.len());
    }

    #[test]
    fn fallback_only_registry_compiles_byte_identical_to_native() {
        let snapshot = RepositorySnapshot::from_files(vec![RepositoryFile::with_content(
            "Cargo.toml",
            "[package]\nname = \"auth\"\n",
        )]);
        let request = ContractCompileRequest {
            goal: "stabilize the parser".to_string(),
            repository: snapshot,
        };
        let native = compile_contract(&request);
        let adapter = SemanticIntelligenceAdapter::new(registry_with(None));
        let compiled = compile_contract_with(&request, Some(&adapter));
        assert_eq!(
            native.contract, compiled.contract,
            "a fallback-only registry must never alter the native contract"
        );
    }
}
