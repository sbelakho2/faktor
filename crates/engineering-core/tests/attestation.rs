use engineering_core::{
    AnalysisIdentity, AnalysisKind, AnalysisReceipt, AnalysisWarning, ArtifactRef, Convergence,
    Digest, EngineeringValue, PropertyRevision, ReceiptContent, ReceiptResult, ReceiptSigner,
    ReceiptValue, Repeatability, RevisionId, SupervisedProcessEvidence, WarningSeverity,
};
use engineering_units::{Quantity, Uncertainty, UnitRegistry};

fn content() -> ReceiptContent {
    let mut content = ReceiptContent::new(
        Digest::of_text("input"),
        Digest::of_text("model"),
        Digest::of_text("settings"),
    );
    content.convergence = Convergence::exact();
    content.warnings = vec![AnalysisWarning {
        code: "fixture".to_string(),
        message: "synthetic attestation fixture".to_string(),
        severity: WarningSeverity::Info,
    }];
    content.results = vec![ReceiptResult {
        name: "result".to_string(),
        value: ReceiptValue::Quantity(
            Quantity::new(1.0, UnitRegistry::si().require("N").unwrap()).unwrap(),
        ),
        uncertainty: Uncertainty::Exact,
    }];
    content.repeatability = Repeatability::deterministic();
    content.boundary_conditions.insert(
        "temperature".to_string(),
        EngineeringValue::known(
            Quantity::new(300.0, UnitRegistry::si().require("K").unwrap()).unwrap(),
            engineering_core::Provenance::new(
                engineering_core::ProvenanceKind::Calculation,
                "attestation fixture",
            )
            .unwrap(),
            RevisionId::new(1),
        )
        .unwrap(),
    );
    content
}

fn kernel(signer: &ReceiptSigner) -> AnalysisReceipt {
    let mut receipt = AnalysisReceipt::kernel(
        AnalysisIdentity {
            solver_id: "kernel-attestation".to_string(),
            solver_version: "1.0.0".to_string(),
            kind: AnalysisKind::InProcessKernel,
        },
        content(),
        1,
    )
    .unwrap();
    signer.sign_receipt(&mut receipt).unwrap();
    receipt
}

fn key() -> ReceiptSigner {
    ReceiptSigner::from_seed([11u8; 32])
}

#[test]
fn signed_kernel_receipt_passes_and_is_canonical() {
    let signer = key();
    let receipt = kernel(&signer);
    receipt.verify_attestation().unwrap();
    assert_eq!(
        receipt.canonical_digest().unwrap(),
        receipt.canonical_digest().unwrap()
    );
    let decoded: AnalysisReceipt =
        serde_json::from_slice(&serde_json::to_vec(&receipt).unwrap()).unwrap();
    assert_eq!(
        decoded.canonical_digest().unwrap(),
        receipt.canonical_digest().unwrap()
    );
    decoded.verify_attestation().unwrap();
}

#[test]
fn unsigned_receipt_is_refused() {
    let mut receipt = kernel(&key());
    receipt.attestation = None;
    assert!(matches!(
        receipt.verify_attestation(),
        Err(engineering_core::CoreError::MissingReceiptAttestation)
    ));
}

#[test]
fn tampered_result_digest_is_refused() {
    let mut receipt = kernel(&key());
    receipt.results[0].value = ReceiptValue::Text("forged".to_string());
    assert!(matches!(
        receipt.verify_attestation(),
        Err(engineering_core::CoreError::InvalidReceiptAttestation { .. })
    ));
    assert!(receipt.validate().is_err());
}

#[test]
fn tampered_solver_version_is_refused() {
    let mut receipt = kernel(&key());
    receipt.identity.solver_version = "9.9.9".to_string();
    assert!(receipt.verify_attestation().is_err());
}

#[test]
fn a_signature_from_another_key_is_refused() {
    let receipt = kernel(&key());
    let other = ReceiptSigner::from_seed([12u8; 32]);
    let verifier = engineering_core::ReceiptVerifier::trusting_key_id(&other.key_id()).unwrap();
    assert!(matches!(
        verifier.verify(&receipt),
        Err(engineering_core::CoreError::UntrustedReceiptKey { .. })
    ));
    let same = engineering_core::ReceiptVerifier::trusting_key_id(&key().key_id()).unwrap();
    same.verify(&receipt).unwrap();
}

#[test]
fn tampered_signature_bytes_are_refused() {
    let mut receipt = kernel(&key());
    let attestation = receipt.attestation.as_mut().unwrap();
    let mut bytes = attestation.signature.clone().into_bytes();
    bytes[0] = if bytes[0] == b'0' { b'1' } else { b'0' };
    attestation.signature = String::from_utf8(bytes).unwrap();
    assert!(matches!(
        receipt.verify_attestation(),
        Err(engineering_core::CoreError::ReceiptSignatureMismatch)
    ));
}

#[test]
fn external_solver_receipt_attests_adapter_and_output_digest() {
    let signer = key();
    let output = Digest::of_text("captured tool output");
    let mut content = content();
    content.artifacts.push(ArtifactRef {
        name: "process-output".to_string(),
        digest: output,
        byte_len: 19,
        media_type: "text/plain".to_string(),
    });
    let mut receipt = AnalysisReceipt::supervised(
        AnalysisIdentity {
            solver_id: "external-adapter".to_string(),
            solver_version: "2.0.0+tool/9.1".to_string(),
            kind: AnalysisKind::SupervisedProcess,
        },
        content,
        SupervisedProcessEvidence {
            command: "tool --run".to_string(),
            exit_code: 0,
            output_digest: output,
            timed_out: false,
            output_truncated: false,
            deadline_ms: 1_000,
        },
    )
    .unwrap();
    signer.sign_receipt(&mut receipt).unwrap();
    receipt.verify_attestation().unwrap();

    if let engineering_core::ExecutionEvidence::SupervisedProcess { output_digest, .. } =
        &mut receipt.execution
    {
        *output_digest = Digest::of_text("rewritten output");
    }
    assert!(receipt.verify_attestation().is_err());
}

#[cfg(unix)]
#[test]
fn signing_key_file_is_owner_only_and_permission_tampering_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let signer = engineering_core::load_or_create_signing_key(dir.path()).unwrap();
    let path = engineering_core::signing_key_path(dir.path());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    engineering_core::load_signing_key(dir.path()).unwrap();

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let error = engineering_core::load_signing_key(dir.path()).unwrap_err();
    assert!(error.to_string().contains("0600"), "{error}");

    // The key is a seed, not an exported public fixture.
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 32);
    assert!(engineering_core::load_or_create_signing_key(dir.path()).is_err());
    assert_eq!(signer.key_id().len(), 64);
}

#[cfg(unix)]
#[test]
fn a_missing_key_is_not_silently_minted_for_verification() {
    let dir = tempfile::tempdir().unwrap();
    assert!(engineering_core::load_signing_key(dir.path()).is_err());
    assert!(!engineering_core::signing_key_path(dir.path()).exists());
    engineering_core::load_or_create_signing_key(dir.path()).unwrap();
    assert!(engineering_core::signing_key_path(dir.path()).exists());
}

#[test]
fn property_revisions_and_boundary_conditions_are_in_the_signed_digest() {
    let signer = key();
    let mut receipt = kernel(&signer);
    receipt
        .property_revisions
        .push(PropertyRevision::new("proof_strength", RevisionId::new(2)).unwrap());
    assert!(receipt.verify_attestation().is_err());
}
