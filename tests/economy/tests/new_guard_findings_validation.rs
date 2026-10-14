//! NEW-GUARD adversarial verification (findings layer, owned by the mutation
//! campaign): the contract compiler re-validates every provider finding
//! against the consult's OWN identity before it can ever become a statement
//! (`crates/core/src/contract_compile.rs`). A finding bound to another
//! snapshot or carried under another provider's name is an UNVALIDATED
//! finding: it must be refused typed, counted as rejected and never applied —
//! and the pure compile stays byte-identical to the no-findings baseline.
//!
//! Mutation oracle (spec under `scripts/mutations/`):
//! - `INV-FINDING-DIGEST-BINDING` (digest/snapshot validation skipped);
//! - `INV-FINDING-PROVIDER-IDENTITY` (provider identity validation skipped).

use faktor_core::contract_compile::{
    compile_contract, compile_contract_with_findings_checked, finding_to_statement,
    findings_to_statements, ContractCompileRequest, FindingDiagnosticCode, FindingKind,
    FindingRejection, FindingsResponse, IntelligenceQuestion, ProviderFinding, RepositoryFile,
    RepositorySnapshot, StatementOrigin, MAX_FINDING_BASIS_BYTES, MAX_FINDING_TEXT_BYTES,
};

const PROVIDER: &str = "semantic";

/// A wired gate granting every value entitlement: the production-embedder
/// entry `compile_contract_with_findings_checked` is the ONLY public apply
/// path (the raw apply is private), so the org-contract entitlement must be
/// presented here.
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

/// Compile the findings through the ONLY public entry (the org-contract
/// value gate is an enforced parameter).
fn compile_checked(
    request: &ContractCompileRequest,
    response: &FindingsResponse,
) -> faktor_core::CompiledContract {
    compile_contract_with_findings_checked(request, PROVIDER, response, &granting_gate())
        .expect("the granting gate admits the apply")
}

fn digest_a() -> String {
    "a".repeat(64)
}

fn digest_b() -> String {
    "b".repeat(64)
}

fn finding(
    question: IntelligenceQuestion,
    kind: FindingKind,
    provider: &str,
    digest: &str,
) -> ProviderFinding {
    ProviderFinding {
        question,
        kind,
        text: format!("{} answer from the validated index", question.as_str()),
        basis: "archived semantic query at revision rev-1".into(),
        confidence_bps: 4_200,
        repository_digest: digest.to_string(),
        provider: provider.to_string(),
        revision: Some("rev-1".into()),
        severity: None,
    }
}

fn request() -> ContractCompileRequest {
    ContractCompileRequest {
        goal: "add a bounded transition guard to the settlement lifecycle".into(),
        repository: RepositorySnapshot::from_files(vec![RepositoryFile::with_content(
            "src/lib.rs",
            "pub fn settle() {}\n",
        )]),
    }
}

/// A finding bound to a DIFFERENT snapshot digest than the consult's is
/// refused typed, counted rejected, and never reaches the compiled contract
/// (which stays byte-identical to the native baseline).
#[test]
fn finding_bound_to_another_snapshot_is_rejected_typed() {
    let expected = digest_a();
    let mut mismatched = finding(
        IntelligenceQuestion::AffectedComponents,
        FindingKind::Component,
        PROVIDER,
        &digest_b(),
    );
    let rejection = finding_to_statement(&mismatched, PROVIDER, &expected).unwrap_err();
    match rejection {
        FindingRejection::DigestMismatch {
            expected: e,
            actual,
        } => {
            assert_eq!(e, expected);
            assert_eq!(actual, mismatched.repository_digest);
        }
        other => panic!("expected a digest rejection, got {other:?}"),
    }

    let conversion = findings_to_statements(PROVIDER, &expected, 8, &[mismatched.clone()]);
    assert_eq!(
        conversion.statements.len(),
        0,
        "an unvalidated finding must never become a statement"
    );
    assert_eq!(conversion.rejected, 1);
    assert!(conversion
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::SnapshotMismatch));

    let compiled = compile_checked(
        &request(),
        &FindingsResponse {
            findings: vec![mismatched.clone()],
            diagnostics: vec![],
        },
    );
    assert_eq!(compiled.enrichments.len(), 1);
    assert_eq!(
        compiled.enrichments[0].applied, 0,
        "an unvalidated finding must never be applied"
    );
    assert_eq!(compiled.enrichments[0].skipped, 1);
    assert!(
        !compiled
            .provenance
            .iter()
            .any(|entry| matches!(entry.origin, StatementOrigin::External { .. })),
        "no external provenance may exist for a rejected finding"
    );
    assert_eq!(
        compiled.contract,
        compile_contract(&request()).contract,
        "rejected findings must leave the native contract byte-identical"
    );

    // The same digest under the OTHER provider's name is rejected too.
    mismatched.repository_digest = digest_a();
    mismatched.provider = "other-provider".into();
    let rejection = finding_to_statement(&mismatched, PROVIDER, &expected).unwrap_err();
    assert!(matches!(
        rejection,
        FindingRejection::ProviderMismatch { .. }
    ));
}

/// A finding claiming ANOTHER provider's identity is refused typed and never
/// applied (defense in depth against a mislabeled answer).
#[test]
fn findings_claiming_another_provider_are_rejected_typed() {
    let expected = digest_a();
    let mislabeled = finding(
        IntelligenceQuestion::Invariants,
        FindingKind::Invariant,
        "not-the-provider-asked",
        &expected,
    );
    let rejection = finding_to_statement(&mislabeled, PROVIDER, &expected).unwrap_err();
    match rejection {
        FindingRejection::ProviderMismatch {
            expected: e,
            actual,
        } => {
            assert_eq!(e, PROVIDER);
            assert_eq!(actual, "not-the-provider-asked");
        }
        other => panic!("expected a provider rejection, got {other:?}"),
    }
    let conversion =
        findings_to_statements(PROVIDER, &expected, 8, std::slice::from_ref(&mislabeled));
    assert_eq!(conversion.statements.len(), 0);
    assert_eq!(conversion.rejected, 1);
    assert!(conversion
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::ProviderMismatch));

    let compiled = compile_checked(
        &request(),
        &FindingsResponse {
            findings: vec![mislabeled],
            diagnostics: vec![],
        },
    );
    assert_eq!(compiled.enrichments[0].applied, 0);
    assert_eq!(compiled.enrichments[0].skipped, 1);
    assert_eq!(compiled.contract, compile_contract(&request()).contract);
}

/// Exhaustive over every rejection kind: a VALID finding is accepted and
/// applied with external provenance; each hostile variant is refused with its
/// typed code and applied nowhere.
#[test]
fn finding_validation_is_exhaustive_over_rejection_kinds() {
    let request = request();
    // The compile's OWN derived digest is the binding the production path
    // checks findings against.
    let expected = compile_contract(&request).repository_digest;
    let valid = finding(
        IntelligenceQuestion::AffectedComponents,
        FindingKind::Component,
        PROVIDER,
        &expected,
    );
    let statement = finding_to_statement(&valid, PROVIDER, &expected).expect("valid finding");
    assert_eq!(statement.confidence_bps, 4_200);
    let compiled = compile_checked(
        &request,
        &FindingsResponse {
            findings: vec![valid.clone()],
            diagnostics: vec![],
        },
    );
    assert_eq!(
        compiled.enrichments[0].applied, 1,
        "{:?}",
        compiled.enrichments[0]
    );
    assert!(compiled
        .provenance
        .iter()
        .any(|entry| matches!(&entry.origin, StatementOrigin::External { provider } if provider == PROVIDER)));

    let long_text = "x".repeat(MAX_FINDING_TEXT_BYTES + 1);
    let long_basis = "x".repeat(MAX_FINDING_BASIS_BYTES + 1);
    let long_revision = "r".repeat(129);
    let cases: Vec<(&str, ProviderFinding, FindingRejection)> = vec![
        (
            "kind mismatch",
            ProviderFinding {
                kind: FindingKind::Risk,
                ..valid.clone()
            },
            FindingRejection::KindMismatch {
                question: IntelligenceQuestion::AffectedComponents,
                kind: FindingKind::Risk,
            },
        ),
        (
            "empty text",
            ProviderFinding {
                text: "   ".into(),
                ..valid.clone()
            },
            FindingRejection::EmptyText,
        ),
        (
            "oversized text",
            ProviderFinding {
                text: long_text,
                ..valid.clone()
            },
            FindingRejection::TextTooLong {
                bytes: MAX_FINDING_TEXT_BYTES + 1,
                max: MAX_FINDING_TEXT_BYTES,
            },
        ),
        (
            "empty basis",
            ProviderFinding {
                basis: String::new(),
                ..valid.clone()
            },
            FindingRejection::EmptyBasis,
        ),
        (
            "oversized basis",
            ProviderFinding {
                basis: long_basis,
                ..valid.clone()
            },
            FindingRejection::BasisTooLong {
                bytes: MAX_FINDING_BASIS_BYTES + 1,
                max: MAX_FINDING_BASIS_BYTES,
            },
        ),
        (
            "confidence out of range",
            ProviderFinding {
                confidence_bps: 10_001,
                ..valid.clone()
            },
            FindingRejection::ConfidenceOutOfRange { bps: 10_001 },
        ),
        (
            "oversized revision",
            ProviderFinding {
                revision: Some(long_revision),
                ..valid.clone()
            },
            FindingRejection::RevisionTooLong {
                bytes: 129,
                max: 128,
            },
        ),
    ];

    let mut hostile = Vec::new();
    for (name, hostile_finding, expected_rejection) in &cases {
        let rejection = finding_to_statement(hostile_finding, PROVIDER, &expected)
            .expect_err(&format!("{name} must be rejected"));
        assert_eq!(
            rejection.code(),
            expected_rejection.code(),
            "{name}: rejection code drifted ({rejection:?})"
        );
        hostile.push(hostile_finding.clone());
    }

    let conversion = findings_to_statements(PROVIDER, &expected, 8, &hostile);
    assert_eq!(
        conversion.statements.len(),
        0,
        "no hostile finding may ever become a statement"
    );
    assert_eq!(conversion.rejected, cases.len());
    let compiled = compile_checked(
        &request,
        &FindingsResponse {
            findings: hostile,
            diagnostics: vec![],
        },
    );
    assert_eq!(compiled.enrichments[0].applied, 0);
    assert_eq!(compiled.enrichments[0].skipped, cases.len());
    assert_eq!(compiled.contract, compile_contract(&request).contract);
}

/// The bound is part of the validation: findings past `max_findings` are
/// rejected and the excess never reaches the contract.
#[test]
fn finding_batch_bound_rejects_the_excess_never_partially_applies_it() {
    let expected = digest_a();
    let findings: Vec<ProviderFinding> = (0..5)
        .map(|index| ProviderFinding {
            text: format!("bounded answer {index}"),
            ..finding(
                IntelligenceQuestion::BlastRadius,
                FindingKind::Risk,
                PROVIDER,
                &expected,
            )
        })
        .collect();
    let conversion = findings_to_statements(PROVIDER, &expected, 2, &findings);
    assert_eq!(conversion.statements.len(), 2);
    assert_eq!(conversion.rejected, 3);
    assert!(conversion
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == FindingDiagnosticCode::CapReached));
}
