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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn kernel(signer: &ReceiptSigner) -> AnalysisReceipt {
    kernel_at(signer, now_ms())
}

fn kernel_at(signer: &ReceiptSigner, signed_at_ms: u64) -> AnalysisReceipt {
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
    signer.sign_receipt_at(&mut receipt, signed_at_ms).unwrap();
    receipt
}

fn key() -> ReceiptSigner {
    ReceiptSigner::from_seed([11u8; 32])
}

#[test]
fn signed_kernel_receipt_passes_and_canonical_digest_is_content_bound() {
    let signer = key();
    let receipt = kernel(&signer);
    receipt.verify_attestation().unwrap();
    let canonical = receipt.canonical_digest().unwrap();
    // Breaks if canonicalization ignores results or changes across a serde trip.
    let mut forged = receipt.clone();
    forged.results[0].value = ReceiptValue::Text("forged".to_string());
    assert_ne!(canonical, forged.canonical_digest().unwrap());
    let decoded: AnalysisReceipt =
        serde_json::from_slice(&serde_json::to_vec(&receipt).unwrap()).unwrap();
    assert_eq!(decoded.canonical_digest().unwrap(), canonical);
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
    // Breaks if identity fields are excluded from the canonical signed digest.
    assert!(matches!(
        receipt.verify_attestation(),
        Err(engineering_core::CoreError::InvalidReceiptAttestation { ref reason })
            if reason.contains("canonical receipt digest")
    ));
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
    // Breaks if the adapter's captured output digest is outside the signed canonical form.
    assert!(matches!(
        receipt.verify_attestation(),
        Err(engineering_core::CoreError::InvalidReceiptAttestation { .. })
    ));
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
    // Breaks if a post-signing property revision is excluded from the canonical digest.
    assert!(matches!(
        receipt.verify_attestation(),
        Err(engineering_core::CoreError::InvalidReceiptAttestation { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_symlinked_signing_key_is_refused_without_following_it() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let decoy = dir.path().join("decoy.key");
    std::fs::write(&decoy, [0x5au8; 32]).unwrap();
    std::os::unix::fs::symlink(&decoy, engineering_core::signing_key_path(&data_dir)).unwrap();

    let error = engineering_core::load_signing_key(&data_dir).unwrap_err();
    assert!(
        matches!(error, engineering_core::CoreError::KeyStoreRefused { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("symlink"), "{error}");
    // The refusal never created or touched the link target through a write.
    assert_eq!(std::fs::read(&decoy).unwrap(), [0x5au8; 32]);
}

#[cfg(unix)]
#[test]
fn a_group_or_world_writable_data_dir_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o777)).unwrap();

    let error = engineering_core::load_or_create_signing_key(&data_dir).unwrap_err();
    assert!(
        matches!(error, engineering_core::CoreError::KeyStoreRefused { .. }),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("group/world writable"),
        "{error}"
    );
    assert!(!engineering_core::signing_key_path(&data_dir).exists());

    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    engineering_core::load_or_create_signing_key(&data_dir).unwrap();
}

#[cfg(unix)]
#[test]
fn a_retired_key_verifies_old_receipts_but_cannot_sign_new_ones() {
    let dir = tempfile::tempdir().unwrap();
    let old = engineering_core::load_or_create_signing_key(dir.path()).unwrap();
    let old_key_id = old.key_id();
    let history = engineering_core::load_key_history(dir.path()).unwrap();
    let not_before = history.entry_for(&old_key_id).unwrap().not_before_ms;
    let issued_at = not_before.max(1);
    let old_receipt = kernel_at(&old, issued_at);

    let retired_at = issued_at + 1_000;
    let new = engineering_core::rotate_signing_key(dir.path(), retired_at).unwrap();
    assert_ne!(new.key_id(), old_key_id);
    // The reloaded store now yields the active successor, never the old key.
    let reloaded = engineering_core::load_signing_key(dir.path()).unwrap();
    assert_eq!(reloaded.key_id(), new.key_id());

    let verifier = engineering_core::load_trusted_verifier(dir.path()).unwrap();
    verifier.verify(&old_receipt).unwrap();

    // A receipt presented as signed after retirement is refused even though
    // the signature itself is authentic.
    let late = kernel_at(&old, retired_at + 1);
    let error = verifier.verify(&late).unwrap_err();
    assert!(
        matches!(
            error,
            engineering_core::CoreError::ReceiptKeyOutsideValidity { .. }
        ),
        "{error:?}"
    );

    // A signer reloaded with the retired window refuses to mint at all.
    let retired_signer = old.clone().with_validity(engineering_core::KeyValidity {
        not_before_ms: not_before,
        retired_at_ms: Some(retired_at),
    });
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
    let error = retired_signer
        .sign_receipt_at(&mut receipt, retired_at)
        .unwrap_err();
    assert!(
        matches!(error, engineering_core::CoreError::RetiredSigningKey { .. }),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn rotation_preserves_auditability_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let old = engineering_core::load_or_create_signing_key(dir.path()).unwrap();
    let not_before = engineering_core::load_key_history(dir.path())
        .unwrap()
        .entry_for(&old.key_id())
        .unwrap()
        .not_before_ms;
    let old_receipt = kernel_at(&old, not_before.max(1));
    let retired_at = not_before.max(1) + 10_000;
    let new = engineering_core::rotate_signing_key(dir.path(), retired_at).unwrap();
    let new_receipt = kernel_at(&new, retired_at + 1);

    // Simulate a process restart: drop every handle and load the trust set
    // and both receipts from durable state only.
    drop(old);
    drop(new);
    let verifier = engineering_core::load_trusted_verifier(dir.path()).unwrap();
    verifier.verify(&old_receipt).unwrap();
    verifier.verify(&new_receipt).unwrap();
    let history = engineering_core::load_key_history(dir.path()).unwrap();
    assert_eq!(history.keys.len(), 2);
    assert!(history
        .entry_for(&old_receipt.attestation.as_ref().unwrap().key_id)
        .is_some());
    assert!(history
        .entry_for(&new_receipt.attestation.as_ref().unwrap().key_id)
        .is_some());
}

#[test]
fn signing_keys_are_never_logged_or_exported() {
    let signer = ReceiptSigner::from_seed([0xabu8; 32]);
    let debug = format!("{signer:?}");
    assert!(debug.contains(&signer.key_id()));
    assert!(!debug.contains(&"ab".repeat(32)));

    let receipt = kernel(&signer);
    let decoded = serde_json::to_string(&receipt).unwrap();
    assert!(!decoded.contains(&"ab".repeat(32)));
    assert!(decoded.contains(&signer.public_key_hex()));
}

#[cfg(unix)]
#[test]
fn a_tampered_key_history_fingerprint_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    engineering_core::load_or_create_signing_key(dir.path()).unwrap();
    let path = engineering_core::key_history_path(dir.path());
    let mut history: engineering_core::KeyHistory =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    history.keys[0].key_id = "00".repeat(32);
    std::fs::write(&path, serde_json::to_vec(&history).unwrap()).unwrap();

    let error = engineering_core::load_trusted_verifier(dir.path()).unwrap_err();
    assert!(
        matches!(error, engineering_core::CoreError::KeyStoreRefused { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("fingerprint"), "{error}");
}
