//! The signed proof/provenance ATTESTATION entry — the
//! `signed_proof_provenance` paid value entitlement.
//!
//! "Externally verifiable attestations of finished work" are produced HERE
//! and nowhere else: [`sign_proof_attestation`] is the ONE entry that turns a
//! bounded subject (named work + the digests it rests on) into a signed,
//! digest-bound attestation. The entry consults the wired
//! [`faktor_core::entitlement::ValueGate`] BEFORE it validates or signs
//! anything: a denial is the typed refusal naming the entitlement, its hook
//! and (when resolved) the plan — never an empty or unsigned artifact. With
//! no cloud authority the gate is absent and the entry refuses
//! `cloud_absent` typed; local evidence storage and reads are untouched.
//!
//! The signing authority is INJECTED ([`AttestationSigner`]): the runtime
//! wires its operator-key signer; this crate owns no key material. The
//! canonical payload is deterministic (schema tag, subject, sorted distinct
//! digests) so two callers over the same subject sign identical bytes.

use faktor_core::entitlement::{ValueEntitlement, ValueGate, ValueGateRefusal};

/// The frozen schema tag of the signed payload.
pub const ATTESTATION_SCHEMA: &str = "faktor-proof-attestation/v1";
/// Longest accepted subject name (bytes).
pub const MAX_ATTESTATION_SUBJECT_BYTES: usize = 256;
/// Hard bound on digests one attestation binds.
pub const MAX_ATTESTATION_DIGESTS: usize = 64;

/// The injected signing authority. `sign` receives the canonical payload
/// bytes and returns the signature bytes; a signing failure is typed, never
/// a silently unsigned attestation.
pub trait AttestationSigner: Send + Sync {
    /// Stable key identity carried in the attestation.
    fn key_id(&self) -> &str;

    /// Sign the canonical payload. `Err` is a typed signing failure.
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, String>;
}

/// One bounded attestation subject: the named work plus the 64-hex digests
/// (evidence/CAS/tree identities) the attestation binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationSubject {
    pub subject: String,
    pub digests: Vec<String>,
}

/// One signed, digest-bound attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedAttestation {
    pub schema: &'static str,
    pub subject: String,
    pub digests: Vec<String>,
    pub key_id: String,
    /// BLAKE3 hex digest of the canonical payload (what the signature covers).
    pub payload_digest: String,
    pub signature: Vec<u8>,
}

/// A typed attestation refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationError {
    /// The value-entitlement gate refused (`cloud_absent` offline, or the
    /// plan-based cause) — names the entitlement/hook/cause/plan.
    Refused(ValueGateRefusal),
    /// The subject is malformed (empty/oversized name, too many digests, a
    /// digest that is not 64 lowercase hex).
    Malformed(String),
    /// The injected signer failed.
    Signing(String),
}

impl std::fmt::Display for AttestationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttestationError::Refused(refusal) => write!(f, "{refusal}"),
            AttestationError::Malformed(message) => {
                write!(f, "proof attestation subject malformed: {message}")
            }
            AttestationError::Signing(message) => {
                write!(f, "proof attestation signing failed: {message}")
            }
        }
    }
}

impl std::error::Error for AttestationError {}

impl From<ValueGateRefusal> for AttestationError {
    fn from(refusal: ValueGateRefusal) -> Self {
        AttestationError::Refused(refusal)
    }
}

fn is_hex64(raw: &str) -> bool {
    raw.len() == 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// The canonical payload the signature covers. Deterministic: the schema
/// tag, the subject, then the sorted, deduplicated digests.
pub fn canonical_attestation_payload(subject: &AttestationSubject) -> Vec<u8> {
    let mut digests = subject.digests.clone();
    digests.sort();
    digests.dedup();
    let mut payload = String::new();
    payload.push_str(ATTESTATION_SCHEMA);
    payload.push('\n');
    payload.push_str(&subject.subject);
    for digest in digests {
        payload.push('\n');
        payload.push_str(&digest);
    }
    payload.into_bytes()
}

/// The ONE gated signed-attestation entry.
///
/// Order of operations (the entitlement boundary is FIRST):
/// 1. [`ValueGate::check`] for [`ValueEntitlement::SignedProofProvenance`];
/// 2. structural validation of the subject (bounded, typed);
/// 3. the injected signer signs the canonical payload; the result carries
///    the payload digest so external verifiers can re-derive it.
pub fn sign_proof_attestation(
    signer: &dyn AttestationSigner,
    subject: &AttestationSubject,
    gate: &ValueGate,
) -> Result<SignedAttestation, AttestationError> {
    gate.check(ValueEntitlement::SignedProofProvenance)?;
    if subject.subject.is_empty() || subject.subject.len() > MAX_ATTESTATION_SUBJECT_BYTES {
        return Err(AttestationError::Malformed(format!(
            "subject must be 1..={MAX_ATTESTATION_SUBJECT_BYTES} bytes"
        )));
    }
    if subject.digests.len() > MAX_ATTESTATION_DIGESTS {
        return Err(AttestationError::Malformed(format!(
            "at most {MAX_ATTESTATION_DIGESTS} digests are bound per attestation"
        )));
    }
    if let Some(hostile) = subject.digests.iter().find(|digest| !is_hex64(digest)) {
        return Err(AttestationError::Malformed(format!(
            "digest {hostile:?} is not 64 lowercase hex characters"
        )));
    }
    let mut digests = subject.digests.clone();
    digests.sort();
    digests.dedup();
    let payload = canonical_attestation_payload(subject);
    let payload_digest = blake3::hash(&payload).to_hex().to_string();
    let signature = signer.sign(&payload).map_err(AttestationError::Signing)?;
    Ok(SignedAttestation {
        schema: ATTESTATION_SCHEMA,
        subject: subject.subject.clone(),
        digests,
        key_id: signer.key_id().to_string(),
        payload_digest,
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::entitlement::{ValueEntitlementGate, ValueGateRefusal, ValueRefusalCause};
    use std::sync::Arc;

    #[derive(Debug)]
    struct TestSigner;

    impl AttestationSigner for TestSigner {
        fn key_id(&self) -> &str {
            "test-key"
        }

        fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, String> {
            let mut signature = b"sig:".to_vec();
            signature.extend_from_slice(payload);
            Ok(signature)
        }
    }

    #[derive(Debug)]
    struct Entitled;

    impl ValueEntitlementGate for Entitled {
        fn check_value_entitlement(
            &self,
            _entitlement: ValueEntitlement,
        ) -> Result<(), ValueGateRefusal> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Denied;

    impl ValueEntitlementGate for Denied {
        fn check_value_entitlement(
            &self,
            entitlement: ValueEntitlement,
        ) -> Result<(), ValueGateRefusal> {
            Err(ValueGateRefusal {
                entitlement,
                hook: entitlement.enforcement_hook(),
                cause: ValueRefusalCause::NotInPlan,
                plan_id: Some("local".into()),
                organization_id: Some("org_acme".into()),
                message: format!(
                    "value entitlement {} not granted for organization org_acme: not_in_plan (plan Some(\"local\"))",
                    entitlement.as_str()
                ),
            })
        }
    }

    fn subject() -> AttestationSubject {
        AttestationSubject {
            subject: "task 7 completion".into(),
            digests: vec!["b".repeat(64), "a".repeat(64)],
        }
    }

    #[test]
    fn signing_is_gated_typed_and_never_reaches_the_signer_when_denied() {
        let entitled = ValueGate::wired(Arc::new(Entitled));
        let signed = sign_proof_attestation(&TestSigner, &subject(), &entitled).unwrap();
        assert_eq!(signed.schema, ATTESTATION_SCHEMA);
        assert_eq!(signed.key_id, "test-key");
        assert_eq!(
            signed.digests,
            vec!["a".repeat(64), "b".repeat(64)],
            "digests are sorted and deduplicated"
        );
        let canonical = canonical_attestation_payload(&subject());
        assert_eq!(
            signed.signature,
            {
                let mut expected = b"sig:".to_vec();
                expected.extend_from_slice(&canonical);
                expected
            },
            "the signature covers the canonical payload"
        );
        assert_eq!(
            signed.payload_digest,
            blake3::hash(&canonical).to_hex().to_string()
        );

        // A denied plan is refused typed AT THE ENTRY, naming the plan; the
        // signer is never consulted.
        let refusal =
            sign_proof_attestation(&TestSigner, &subject(), &ValueGate::wired(Arc::new(Denied)))
                .unwrap_err();
        match refusal {
            AttestationError::Refused(refusal) => {
                assert_eq!(refusal.entitlement, ValueEntitlement::SignedProofProvenance);
                assert_eq!(refusal.hook.as_str(), "proof_attestation");
                assert_eq!(refusal.cause, ValueRefusalCause::NotInPlan);
                assert_eq!(refusal.plan_id.as_deref(), Some("local"));
            }
            other => panic!("expected the typed refusal, got {other:?}"),
        }

        // Offline (no cloud authority): typed cloud_absent.
        let refusal =
            sign_proof_attestation(&TestSigner, &subject(), &ValueGate::absent()).unwrap_err();
        match refusal {
            AttestationError::Refused(refusal) => {
                assert_eq!(refusal.cause, ValueRefusalCause::CloudAbsent);
            }
            other => panic!("expected the typed refusal, got {other:?}"),
        }
    }

    #[test]
    fn malformed_subjects_are_typed_refusals() {
        let entitled = ValueGate::wired(Arc::new(Entitled));
        let empty = AttestationSubject {
            subject: String::new(),
            digests: vec![],
        };
        assert!(matches!(
            sign_proof_attestation(&TestSigner, &empty, &entitled),
            Err(AttestationError::Malformed(_))
        ));
        let bad_digest = AttestationSubject {
            subject: "x".into(),
            digests: vec!["Z".repeat(64)],
        };
        assert!(matches!(
            sign_proof_attestation(&TestSigner, &bad_digest, &entitled),
            Err(AttestationError::Malformed(_))
        ));
        let too_many = AttestationSubject {
            subject: "x".into(),
            digests: vec!["a".repeat(64); MAX_ATTESTATION_DIGESTS + 1],
        };
        assert!(matches!(
            sign_proof_attestation(&TestSigner, &too_many, &entitled),
            Err(AttestationError::Malformed(_))
        ));
    }
}
