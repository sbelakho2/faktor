//! @authority-collapse: authority-bearing
//!
//! System-intelligence contract enrichment over the semantic-provider
//! registry: the production adapter for the neutral
//! [`faktor_core::contract_compile::SystemIntelligenceProvider`] seam.
//!
//! The adapter ASKS the registry's validated providers the fixed bounded
//! engineering questions (`docs/specs/autonomous-engineering.md` § "System
//! intelligence") and returns their answers as
//! [`faktor_core::contract_compile::ProviderFinding`] DATA:
//!
//! - Questions are consulted through the registry's own guarded dispatch,
//!   PINNED to the provider whose validated descriptor covers `context` and
//!   `require_provider`-bound, so the generic fallback can never answer on a
//!   provider's behalf and every response is identity-checked (provider,
//!   schema, workspace, snapshot, entity refs, payload bounds, provenance).
//! - Findings are bound to the request's repository digest and to the
//!   provider's source revision; a snapshot/workspace/provenance mismatch,
//!   an oversized answer or a refusal is dropped with a typed
//!   [`FindingDiagnostic`] — never guessed into a claim.
//! - Confidence is clamped by the confidence the shared intelligence mapping
//!   derives from the provider's DECLARED evidence dimensions: a provider
//!   that declared no fidelity contributes nothing (tier never launders
//!   confidence).
//! - [`SemanticIntelligenceAdapter::findings`] is total: it never errors and
//!   never fails a turn; a missing, unvalidated or refusing provider simply
//!   contributes nothing. The pure compiler stays byte-identical when no
//!   findings arrive (Tangerine is never required).

use std::sync::Arc;

use faktor_core::contract_compile::{
    FindingDiagnostic, FindingDiagnosticCode, FindingsRequest, FindingsResponse,
    IntelligenceFuture, IntelligenceQuestion, ProviderFinding, SystemIntelligenceProvider,
    MAX_FINDING_BASIS_BYTES, MAX_FINDING_DIAGNOSTICS, MAX_FINDING_REVISION_BYTES,
    MAX_FINDING_TEXT_BYTES,
};

use crate::quality::{effective_quality, SemanticQualityRequirement};
use crate::registry::SemanticProviderRegistry;
use crate::types::{
    SemanticCall, SemanticContextItem, SemanticContextRequest, SemanticError, SemanticOp,
    SemanticSnapshotId, SEMANTIC_SCHEMA_VERSION,
};

/// Stable provider id of the registry-level adapter: every enriched
/// statement carries `external:semantic` provenance, and the finding BASIS
/// names the concrete semantic provider the answer came from.
pub const SEMANTIC_ENRICHMENT_PROVIDER_ID: &str = "semantic";

/// Hard bound on findings asked per question per provider.
pub const MAX_FINDINGS_PER_QUESTION: usize = 8;
/// Hard bound on one question response's bytes.
pub const MAX_FINDINGS_QUERY_BYTES: usize = 16 * 1024;

/// The production bridge from the daemon's semantic-provider registry to the
/// contract compiler's neutral findings seam. Construct it over the SAME
/// `Arc<SemanticProviderRegistry>` the agent and server introspect (one
/// authority), so the answers can never describe a registry the daemon is
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

    /// The bounded `context` consult one question is asked through: pinned to
    /// the provider whose identity the snapshot was derived from and
    /// provider-required, so a fallback (or another provider) can never
    /// answer a question whose identity describes this one.
    fn question_request(
        &self,
        request: &FindingsRequest<'_>,
        provider_id: &crate::types::SemanticProviderId,
        snapshot_id: SemanticSnapshotId,
        revision: &str,
        question: IntelligenceQuestion,
    ) -> SemanticContextRequest {
        let mut call = SemanticCall::new(
            request.operation_id,
            request.session_id,
            request.workspace,
            request.started_ms,
            request.cancellation.clone(),
        )
        .requiring_provider()
        .pinned_to(provider_id.clone());
        if let Some(deadline) = request.deadline {
            call = call.with_deadline(deadline);
        }
        SemanticContextRequest {
            call,
            workspace: request.workspace,
            source_revision: revision.to_string(),
            snapshot_id,
            query: question.query(request.goal),
            max_items: MAX_FINDINGS_PER_QUESTION,
            max_bytes: MAX_FINDINGS_QUERY_BYTES,
        }
    }
}

impl SystemIntelligenceProvider for SemanticIntelligenceAdapter {
    fn provider_id(&self) -> &str {
        SEMANTIC_ENRICHMENT_PROVIDER_ID
    }

    /// Ask every validated provider the fixed bounded question set. Answers
    /// become bounded findings; every failure along the way becomes a typed
    /// diagnostic and contributes nothing.
    fn findings<'a>(
        &'a self,
        request: &'a FindingsRequest<'a>,
    ) -> IntelligenceFuture<'a, Result<FindingsResponse, String>> {
        Box::pin(async move {
            let mut response = FindingsResponse::default();
            let Some(revision) = request.source_revision else {
                // Identity law: without a content revision there is no
                // snapshot identity to bind questions to — never synthesize
                // one, never ask.
                push_diagnostic(
                    &mut response,
                    SEMANTIC_ENRICHMENT_PROVIDER_ID,
                    FindingDiagnosticCode::RevisionUnavailable,
                    format_args!(
                        "no workspace content revision was established; no question was asked"
                    ),
                );
                return Ok(response);
            };
            if request.max_findings == 0 || request.questions.is_empty() {
                return Ok(response);
            }
            for provider in self.registry.providers() {
                if response.findings.len() >= request.max_findings {
                    break;
                }
                // Unvalidated (e.g. an external provider before its
                // handshake): invisible, exactly like selection.
                let Some(descriptor) = provider.validated_descriptor() else {
                    continue;
                };
                // The ONE quality gate: a single declared-trust requirement
                // (no particular coverage floor) for the context operation.
                // An undeclared fidelity is `None` and cannot pass.
                let requirement = SemanticQualityRequirement::declared(SemanticOp::Context);
                if !descriptor.covers_validated(requirement.capabilities) {
                    continue;
                }
                let quality =
                    effective_quality(provider.as_ref(), &descriptor, SemanticOp::Context);
                if !requirement.accepts(&quality) {
                    continue;
                }
                let ceiling = quality.confidence_bps();
                let provider_id = provider.id();
                let snapshot_id = SemanticSnapshotId::derive(
                    request.workspace,
                    revision,
                    &provider_id,
                    provider.version(),
                    SEMANTIC_SCHEMA_VERSION,
                );
                for question in request.questions {
                    if response.findings.len() >= request.max_findings {
                        break;
                    }
                    let context_request = self.question_request(
                        request,
                        &provider_id,
                        snapshot_id,
                        revision,
                        *question,
                    );
                    match self.registry.context(context_request).await {
                        Ok(envelope) => {
                            for item in &envelope.payload.items {
                                if response.findings.len() >= request.max_findings {
                                    break;
                                }
                                collect_item(
                                    &mut response,
                                    *question,
                                    provider_id.as_str(),
                                    revision,
                                    snapshot_id,
                                    request.repository_digest,
                                    ceiling,
                                    item,
                                );
                            }
                        }
                        Err(error) => {
                            push_diagnostic(
                                &mut response,
                                provider_id.as_str(),
                                diagnostic_code_for(&error),
                                format_args!("question {} failed: {error}", question.as_str()),
                            );
                            // This provider cannot serve the consult (typed
                            // identity/refusal/absence failure): try the next
                            // registered provider instead of re-asking it.
                            break;
                        }
                    }
                }
            }
            response.findings.truncate(request.max_findings);
            Ok(response)
        })
    }
}

/// Convert one retrieved context item into a bounded finding, or a typed
/// diagnostic when the answer cannot be honestly disclosed.
#[allow(clippy::too_many_arguments)]
fn collect_item(
    response: &mut FindingsResponse,
    question: IntelligenceQuestion,
    provider_id: &str,
    revision: &str,
    snapshot_id: SemanticSnapshotId,
    repository_digest: &str,
    ceiling: u16,
    item: &SemanticContextItem,
) {
    let excerpt = item.excerpt.trim();
    if excerpt.is_empty() {
        return;
    }
    if excerpt.len() > MAX_FINDING_TEXT_BYTES {
        push_diagnostic(
            response,
            provider_id,
            FindingDiagnosticCode::Oversized,
            format_args!(
                "question {} answer of {} bytes exceeds the {MAX_FINDING_TEXT_BYTES}-byte finding bound",
                question.as_str(),
                excerpt.len()
            ),
        );
        return;
    }
    let relevance = item.relevance_bps.min(10_000) as u16;
    let confidence_bps = relevance.min(ceiling);
    if confidence_bps == 0 {
        return;
    }
    let basis = bounded(
        &format!(
            "semantic provider {provider_id} answered question {} at {} (relevance {relevance} bps, snapshot {}, revision {revision})",
            question.as_str(),
            item.entity.path,
            snapshot_id.to_hex(),
        ),
        MAX_FINDING_BASIS_BYTES,
    );
    response.findings.push(ProviderFinding {
        question,
        kind: question.expected_kind(),
        text: excerpt.to_string(),
        basis,
        confidence_bps,
        repository_digest: repository_digest.to_string(),
        provider: SEMANTIC_ENRICHMENT_PROVIDER_ID.to_string(),
        revision: Some(bounded(revision, MAX_FINDING_REVISION_BYTES)),
        severity: None,
    });
}

/// The typed diagnostic code of one semantic failure. Snapshot/workspace/
/// provenance violations are their own codes; oversized payloads are
/// `Oversized`; absence is `ProviderUnavailable`; caller deadlines and
/// provider watchdogs are `ProviderTimedOut`.
fn diagnostic_code_for(error: &SemanticError) -> FindingDiagnosticCode {
    match error {
        SemanticError::SnapshotMismatch { .. } => FindingDiagnosticCode::SnapshotMismatch,
        SemanticError::Oversized { .. } => FindingDiagnosticCode::Oversized,
        SemanticError::Cancelled { .. }
        | SemanticError::ProviderRequired { .. }
        | SemanticError::Unavailable { .. } => FindingDiagnosticCode::ProviderUnavailable,
        SemanticError::DeadlineExceeded { .. } | SemanticError::ProviderTimeout { .. } => {
            FindingDiagnosticCode::ProviderTimedOut
        }
        SemanticError::ProviderCrashed { .. }
        | SemanticError::ProviderFailed { .. }
        | SemanticError::QualityBelowRequirement { .. }
        | SemanticError::QualityUnavailable { .. } => FindingDiagnosticCode::ProviderRefused,
        SemanticError::Malformed(_)
        | SemanticError::WorkspaceMismatch { .. }
        | SemanticError::UnsupportedSchema { .. }
        | SemanticError::InvalidEntityRef(_)
        | SemanticError::DuplicateEntity(_)
        | SemanticError::Refused(_)
        | SemanticError::PathTraversal(_)
        | SemanticError::StaleHash { .. }
        | SemanticError::ProviderMismatch { .. }
        | SemanticError::InvalidIntelligenceTier { .. } => FindingDiagnosticCode::InvalidProvenance,
    }
}

/// Append one bounded typed diagnostic, capped at [`MAX_FINDING_DIAGNOSTICS`]
/// (a diagnostic flood must never grow memory).
fn push_diagnostic(
    response: &mut FindingsResponse,
    provider: &str,
    code: FindingDiagnosticCode,
    detail: std::fmt::Arguments<'_>,
) {
    if response.diagnostics.len() >= MAX_FINDING_DIAGNOSTICS {
        return;
    }
    response
        .diagnostics
        .push(FindingDiagnostic::new(provider, code, detail.to_string()));
}

/// Truncate `text` to at most `max` bytes on a character boundary.
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
    use crate::test_support::{block_on, provider_id};
    use crate::types::{
        BoxFuture, SemanticCapabilities, SemanticCompleteness, SemanticContextPack,
        SemanticEntityId, SemanticEntityRef, SemanticEnvelope, SemanticFidelity, SemanticProvider,
        SemanticProviderDescriptor, SemanticProviderId, WorkspacePath,
    };

    use faktor_core::contract::EngineeringContract;
    use faktor_core::contract_compile::{
        compile_contract, compile_contract_with_findings_checked, findings_to_statements,
        ContractCompileRequest, RepositoryFile, RepositorySnapshot, MAX_FINDINGS,
    };
    use faktor_core::{CancellationToken, ContractOrigin, OpId, SessionId, WorkspaceId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A wired gate that grants every value entitlement: the enrichment
    /// tests exercise the org-contract apply path, which is now reachable
    /// only through the checked entry.
    #[derive(Debug)]
    struct Granted;

    impl faktor_core::entitlement::ValueEntitlementGate for Granted {
        fn check_value_entitlement(
            &self,
            _entitlement: faktor_core::entitlement::ValueEntitlement,
        ) -> Result<(), faktor_core::entitlement::ValueGateRefusal> {
            Ok(())
        }
    }

    fn granting_gate() -> faktor_core::entitlement::ValueGate {
        faktor_core::entitlement::ValueGate::wired(std::sync::Arc::new(Granted))
    }

    /// One registry provider that answers the bounded questions out of its
    /// scripted per-question excerpts — exactly the shape a real semantic
    /// service presents over the `context` operation.
    struct AnsweringProvider {
        id: SemanticProviderId,
        fidelity: Option<SemanticFidelity>,
        completeness: Option<SemanticCompleteness>,
        tier: Option<u8>,
        answers: Vec<(IntelligenceQuestion, String, u32)>,
        /// Override the snapshot the envelope claims (mismatch tests).
        snapshot_override: Option<SemanticSnapshotId>,
        /// Mint envelopes with UserPolicy provenance (laundering test).
        launder_provenance: bool,
        /// Answer with a hostile oversized excerpt.
        oversized_excerpt: Option<String>,
        consults: Arc<AtomicUsize>,
    }

    impl AnsweringProvider {
        fn answering() -> Self {
            Self {
                id: provider_id("answers"),
                fidelity: Some(SemanticFidelity::Structural),
                completeness: Some(SemanticCompleteness::ConservativeComplete),
                tier: Some(2),
                answers: vec![
                    (
                        IntelligenceQuestion::AffectedComponents,
                        "auth.refresh owns refresh-token rotation".to_string(),
                        9_000,
                    ),
                    (
                        IntelligenceQuestion::Invariants,
                        "issued tokens stay usable until their expiry".to_string(),
                        8_000,
                    ),
                    (
                        IntelligenceQuestion::HiddenDependencies,
                        "billing depends on token introspection".to_string(),
                        7_500,
                    ),
                    (
                        IntelligenceQuestion::BlastRadius,
                        "session middleware breaks if the refresh shape changes".to_string(),
                        8_500,
                    ),
                    (
                        IntelligenceQuestion::HistoricalFailures,
                        "a 2024 migration double-issued refresh tokens".to_string(),
                        6_000,
                    ),
                ],
                snapshot_override: None,
                launder_provenance: false,
                oversized_excerpt: None,
                consults: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn snapshot_mismatched() -> Self {
            let mut provider = Self::answering();
            provider.snapshot_override = Some(SemanticSnapshotId::from_file_hash(
                faktor_core::FileHash::from([7u8; 32]),
            ));
            provider
        }

        fn laundering() -> Self {
            let mut provider = Self::answering();
            provider.launder_provenance = true;
            provider
        }

        fn oversized() -> Self {
            let mut provider = Self::answering();
            provider.oversized_excerpt = Some("x".repeat(MAX_FINDING_TEXT_BYTES + 64));
            provider
        }

        fn question_of(query: &str) -> Option<IntelligenceQuestion> {
            IntelligenceQuestion::ALL
                .into_iter()
                .find(|question| query.contains(&format!("question {}", question.as_str())))
        }
    }

    impl SemanticProvider for AnsweringProvider {
        fn id(&self) -> SemanticProviderId {
            self.id.clone()
        }

        fn version(&self) -> u32 {
            1
        }

        fn capabilities(&self) -> SemanticCapabilities {
            SemanticCapabilities::CONTEXT
        }

        fn descriptor(&self) -> SemanticProviderDescriptor {
            let mut descriptor = SemanticProviderDescriptor::new(
                self.id.clone(),
                self.version(),
                SEMANTIC_SCHEMA_VERSION,
                self.capabilities(),
            );
            descriptor.fidelity = self.fidelity;
            descriptor.completeness = self.completeness;
            descriptor.tier = self.tier;
            descriptor
        }

        fn context(
            &self,
            request: SemanticContextRequest,
        ) -> BoxFuture<'_, Result<SemanticEnvelope<SemanticContextPack>, SemanticError>> {
            self.consults.fetch_add(1, Ordering::SeqCst);
            let question = Self::question_of(&request.query);
            let mut items = Vec::new();
            if let Some(question) = question {
                if let Some(oversized) = &self.oversized_excerpt {
                    items.push(item(
                        request.workspace,
                        "src/auth/refresh.rs",
                        oversized,
                        9_000,
                    ));
                } else {
                    for (answer_question, excerpt, relevance) in &self.answers {
                        if *answer_question == question {
                            items.push(item(
                                request.workspace,
                                "src/auth/refresh.rs",
                                excerpt,
                                *relevance,
                            ));
                        }
                    }
                }
            }
            let snapshot_id = self.snapshot_override.unwrap_or(request.snapshot_id);
            let mut envelope = SemanticEnvelope::new(
                self.id.clone(),
                self.version(),
                request.workspace,
                snapshot_id,
                7,
                SemanticContextPack {
                    total_bytes: items.iter().map(|i| i.excerpt.len()).sum(),
                    items,
                    truncated: false,
                    degraded: false,
                    unavailable: Vec::new(),
                },
            );
            if self.launder_provenance {
                envelope.provenance = faktor_evidence::types::ProvenanceSet::new([
                    faktor_evidence::types::ProvenanceSource::UserPolicy,
                ]);
            }
            Box::pin(async move { Ok(envelope) })
        }
    }

    fn item(
        workspace: WorkspaceId,
        path: &str,
        excerpt: &str,
        relevance_bps: u32,
    ) -> SemanticContextItem {
        let entity = SemanticEntityRef::new(
            workspace,
            WorkspacePath::parse(path).expect("valid path"),
            SemanticEntityId::parse("auth.refresh").expect("valid entity"),
        );
        SemanticContextItem {
            entity,
            relevance_bps,
            excerpt: excerpt.to_string(),
            provenance: Vec::new(),
        }
    }

    fn registry_with(
        provider: Option<AnsweringProvider>,
    ) -> (Arc<SemanticProviderRegistry>, Arc<AtomicUsize>) {
        let mut registry = SemanticProviderRegistry::new(GenericSemanticFallback::default());
        let consults = provider
            .as_ref()
            .map(|provider| provider.consults.clone())
            .unwrap_or_default();
        if let Some(provider) = provider {
            registry.register(Arc::new(provider));
        }
        (Arc::new(registry), consults)
    }

    fn findings_request<'a>(
        contract: &'a EngineeringContract,
        tokens: &'a [String],
        build_systems: &'a [String],
        goal: &'a str,
        digest: &'a str,
        revision: Option<&'a str>,
    ) -> FindingsRequest<'a> {
        FindingsRequest {
            operation_id: OpId::new(1),
            session_id: SessionId::new(1),
            workspace: WorkspaceId::new(1),
            started_ms: 0,
            deadline: None,
            cancellation: CancellationToken::new(),
            source_revision: revision,
            goal,
            intent_tokens: tokens,
            build_systems,
            repository_digest: digest,
            native_contract: contract,
            questions: &IntelligenceQuestion::ALL,
            max_findings: MAX_FINDINGS,
        }
    }

    fn consult_with_digest(
        registry: &Arc<SemanticProviderRegistry>,
        digest: &str,
    ) -> FindingsResponse {
        let contract = EngineeringContract::default();
        let tokens: Vec<String> = Vec::new();
        let build_systems: Vec<String> = vec!["cargo".to_string()];
        let adapter = SemanticIntelligenceAdapter::new(registry.clone());
        let request = findings_request(
            &contract,
            &tokens,
            &build_systems,
            "fix the refresh token race",
            digest,
            Some("rev-1"),
        );
        block_on(adapter.findings(&request)).expect("the adapter is total")
    }

    fn consult(registry: &Arc<SemanticProviderRegistry>) -> FindingsResponse {
        consult_with_digest(registry, "repo2:digest")
    }

    #[test]
    fn validated_provider_answers_the_real_questions_with_typed_findings() {
        let (registry, consults) = registry_with(Some(AnsweringProvider::answering()));
        let response = consult(&registry);
        assert!(
            response.diagnostics.is_empty(),
            "{:?}",
            response.diagnostics
        );
        assert_eq!(response.findings.len(), 5);
        assert_eq!(
            consults.load(Ordering::SeqCst),
            5,
            "one bounded consult per question"
        );
        for question in IntelligenceQuestion::ALL {
            let finding = response
                .findings
                .iter()
                .find(|finding| finding.question == question)
                .unwrap_or_else(|| panic!("question {} must be answered", question.as_str()));
            assert_eq!(finding.kind, question.expected_kind());
            assert_eq!(finding.provider, SEMANTIC_ENRICHMENT_PROVIDER_ID);
            assert_eq!(finding.repository_digest, "repo2:digest");
            assert!(finding.confidence_bps > 0);
            assert!(finding.basis.contains("answers"));
            // No capability disclosure ever appears in a finding.
            assert!(!finding.text.contains("declares"));
        }
        assert!(response.findings.iter().any(|finding| finding
            .text
            .contains("auth.refresh owns refresh-token rotation")));
    }

    #[test]
    fn findings_feed_the_contract_as_provenance_tagged_statements() {
        let (registry, _) = registry_with(Some(AnsweringProvider::answering()));
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
        // The questions are bound to THIS repository snapshot identity.
        let response = consult_with_digest(&registry, &native.repository_digest);
        let compiled = compile_contract_with_findings_checked(
            &request,
            "semantic",
            &response,
            &granting_gate(),
        )
        .expect("the entitled org-contract gate applies the findings");
        // Native statements survive; external statements are the findings.
        for obligation in &native.contract.proof_obligations {
            assert!(compiled
                .contract
                .proof_obligations
                .iter()
                .any(|o| o == obligation));
        }
        let external_risks: Vec<&faktor_core::RiskHypothesis> = compiled
            .contract
            .risks
            .iter()
            .filter(|risk| matches!(risk.origin, ContractOrigin::External { .. }))
            .collect();
        assert!(external_risks
            .iter()
            .any(|risk| risk.description.contains("affected component: ")));
        assert!(external_risks
            .iter()
            .any(|risk| risk.description.contains("hidden dependency: ")));
        assert!(external_risks
            .iter()
            .any(|risk| risk.description.contains("blast radius: ")));
        assert!(external_risks
            .iter()
            .any(|risk| risk.description.contains("historical failure: ")));
        assert!(compiled
            .contract
            .invariants
            .iter()
            .any(|invariant| invariant
                .statement
                .contains("invariant that must survive: ")));
        assert_eq!(compiled.enrichments[0].provider, "semantic");
        assert_eq!(compiled.enrichments[0].applied, 5);
        assert_eq!(compiled.enrichments[0].skipped, 0);
        // The old descriptor-disclosure text is gone.
        for risk in &compiled.contract.risks {
            assert!(!risk.description.contains("declares"));
        }
    }

    #[test]
    fn snapshot_mismatch_is_rejected_typed_and_contributes_nothing() {
        let (registry, _) = registry_with(Some(AnsweringProvider::snapshot_mismatched()));
        let response = consult(&registry);
        assert!(response.findings.is_empty());
        assert!(
            response
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::SnapshotMismatch),
            "{:?}",
            response.diagnostics
        );
    }

    #[test]
    fn provenance_laundering_is_rejected_typed() {
        let (registry, _) = registry_with(Some(AnsweringProvider::laundering()));
        let response = consult(&registry);
        assert!(response.findings.is_empty());
        assert!(
            response
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::InvalidProvenance),
            "{:?}",
            response.diagnostics
        );
    }

    #[test]
    fn oversized_answers_are_bounded_with_a_typed_diagnostic() {
        let (registry, _) = registry_with(Some(AnsweringProvider::oversized()));
        let response = consult(&registry);
        assert!(response.findings.is_empty());
        assert!(
            response
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::Oversized),
            "{:?}",
            response.diagnostics
        );
    }

    #[test]
    fn fallback_only_registry_answers_nothing() {
        let (registry, consults) = registry_with(None);
        let response = consult(&registry);
        assert!(response.findings.is_empty());
        assert!(response.diagnostics.is_empty());
        assert_eq!(consults.load(Ordering::SeqCst), 0);
        // The adapter is also honest about its provenance id.
        let adapter = SemanticIntelligenceAdapter::new(registry);
        assert_eq!(adapter.provider_id(), "semantic");
    }

    #[test]
    fn undeclared_fidelity_provider_is_invisible() {
        let mut provider = AnsweringProvider::answering();
        provider.fidelity = None;
        let (registry, consults) = registry_with(Some(provider));
        let response = consult(&registry);
        assert!(
            response.findings.is_empty(),
            "a provider with no declared evidence contributes zero findings"
        );
        assert_eq!(
            consults.load(Ordering::SeqCst),
            0,
            "an undeclared-fidelity provider is never asked"
        );
    }

    #[test]
    fn missing_content_revision_asks_no_question() {
        let (registry, consults) = registry_with(Some(AnsweringProvider::answering()));
        let contract = EngineeringContract::default();
        let tokens: Vec<String> = Vec::new();
        let build_systems: Vec<String> = vec!["cargo".to_string()];
        let adapter = SemanticIntelligenceAdapter::new(registry);
        let request = findings_request(
            &contract,
            &tokens,
            &build_systems,
            "fix the refresh token race",
            "repo2:digest",
            None,
        );
        let response = block_on(adapter.findings(&request)).expect("total");
        assert!(response.findings.is_empty());
        assert!(response
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::RevisionUnavailable));
        assert_eq!(consults.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn zero_finding_cap_asks_nothing() {
        let (registry, consults) = registry_with(Some(AnsweringProvider::answering()));
        let contract = EngineeringContract::default();
        let tokens: Vec<String> = Vec::new();
        let build_systems: Vec<String> = vec!["cargo".to_string()];
        let adapter = SemanticIntelligenceAdapter::new(registry);
        let mut request = findings_request(
            &contract,
            &tokens,
            &build_systems,
            "g",
            "repo2:digest",
            Some("rev-1"),
        );
        request.max_findings = 0;
        let response = block_on(adapter.findings(&request)).expect("total");
        assert!(response.findings.is_empty());
        assert_eq!(consults.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn conversion_of_adapter_findings_rejects_a_foreign_digest() {
        let (registry, _) = registry_with(Some(AnsweringProvider::answering()));
        let response = consult(&registry);
        // The same answers bound to another snapshot compile nothing.
        let conversion = findings_to_statements("semantic", "repo2:other", 24, &response.findings);
        assert!(conversion.statements.is_empty());
        assert!(conversion
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code == FindingDiagnosticCode::SnapshotMismatch));
    }
}
