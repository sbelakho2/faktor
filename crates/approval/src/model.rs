use std::fmt;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Signer as _, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Maximum bytes in a principal identifier.
pub const MAX_PRINCIPAL_ID_BYTES: usize = 128;
/// Maximum bytes in a free-text approval field.
pub const MAX_APPROVAL_TEXT_BYTES: usize = 4_096;
/// Maximum evidence references on one request.
pub const MAX_APPROVAL_EVIDENCE: usize = 64;
/// Maximum aggregate bytes across all evidence references of one request.
pub const MAX_APPROVAL_EVIDENCE_BYTES: usize = 16 * 1024;
/// Maximum OPEN (undecided, unexpired) requests in one ledger. A bounded
/// queue keeps a long task's pending approvals finite; deciding or letting
/// requests expire frees capacity (the ledger itself is append-only).
pub const MAX_OPEN_APPROVALS: usize = 128;
/// Maximum lifetime of one request (30 days).
pub const MAX_REQUEST_LIFETIME_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

const REQUEST_DOMAIN: &[u8] = b"faktor.approval.request/v1";
const DECISION_DOMAIN: &[u8] = b"faktor.approval.decision/v1";
const DECISION_SIGNATURE_DOMAIN: &[u8] = b"faktor.approval.decision-signature/v1";

/// The consequential action class an approval authorizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Relax a numeric engineering constraint (e.g. tolerance ±10 -> ±25 µm).
    EngineeringConstraintRelaxation,
    /// Change an engineering requirement or acceptance criterion.
    EngineeringAcceptanceCriteriaChange,
    /// Waive or downgrade a proof/evidence obligation.
    EngineeringProofObligationWaiver,
    /// Accept a model outside its declared validity envelope.
    EngineeringModelValidityOverride,
    /// Change a previously verified design constraint.
    EngineeringVerifiedConstraintChange,
    /// Override a should-cost assumption.
    SupplyShouldCostAssumptionOverride,
    /// Approve/qualify a supplier or change its qualification state.
    SupplySupplierQualification,
    /// Change the approved vendor list / supplier authorization.
    SupplyAvlChange,
    /// Accept an RFQ or its commercial terms.
    SupplyRfqAcceptance,
    /// Create, amend or cancel a purchase order commitment.
    SupplyPurchaseOrder,
    /// Send a message to a supplier/counterparty.
    SupplySupplierMessage,
    /// Accept or change payment terms.
    SupplyPaymentTerms,
    /// Accept or change contract terms.
    SupplyContractTerms,
    /// Accept an ambiguous HS/tariff classification.
    SupplyTariffClassification,
    /// Change inventory policy or safety stock above threshold.
    SupplyInventoryPolicy,
    /// Proceed with a SupplyDecision that changes an approved baseline.
    SupplyPlanProceed,
}

impl ApprovalKind {
    /// Stable snake_case label used in errors and JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EngineeringConstraintRelaxation => "engineering_constraint_relaxation",
            Self::EngineeringAcceptanceCriteriaChange => "engineering_acceptance_criteria_change",
            Self::EngineeringProofObligationWaiver => "engineering_proof_obligation_waiver",
            Self::EngineeringModelValidityOverride => "engineering_model_validity_override",
            Self::EngineeringVerifiedConstraintChange => "engineering_verified_constraint_change",
            Self::SupplyShouldCostAssumptionOverride => "supply_should_cost_assumption_override",
            Self::SupplySupplierQualification => "supply_supplier_qualification",
            Self::SupplyAvlChange => "supply_avl_change",
            Self::SupplyRfqAcceptance => "supply_rfq_acceptance",
            Self::SupplyPurchaseOrder => "supply_purchase_order",
            Self::SupplySupplierMessage => "supply_supplier_message",
            Self::SupplyPaymentTerms => "supply_payment_terms",
            Self::SupplyContractTerms => "supply_contract_terms",
            Self::SupplyTariffClassification => "supply_tariff_classification",
            Self::SupplyInventoryPolicy => "supply_inventory_policy",
            Self::SupplyPlanProceed => "supply_plan_proceed",
        }
    }
}

impl fmt::Display for ApprovalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The risk class a human reviewer sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Bounded, reversible.
    Low,
    /// Material but contained.
    Medium,
    /// Material with external consequences.
    High,
    /// Safety, legal or large financial exposure.
    Critical,
}

/// Who is asking or deciding. Only [`PrincipalKind::HumanOperator`] may
/// decide; agents and services may only request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A named human operator holding a registered Ed25519 key.
    HumanOperator,
    /// An autonomous agent.
    Agent,
    /// An automated service.
    Service,
}

impl PrincipalKind {
    /// Only a human operator carries approval authority.
    pub const fn can_approve(self) -> bool {
        matches!(self, Self::HumanOperator)
    }
}

/// One principal (requester or decider).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    /// Stable principal id.
    pub id: String,
    /// Principal class.
    pub kind: PrincipalKind,
}

impl Principal {
    /// A bounded, validated principal.
    pub fn new(id: impl Into<String>, kind: PrincipalKind) -> Result<Self, ApprovalError> {
        let principal = Self {
            id: id.into(),
            kind,
        };
        principal.validate()?;
        Ok(principal)
    }

    /// Validate the identifier.
    pub fn validate(&self) -> Result<(), ApprovalError> {
        if self.id.is_empty() || self.id.len() > MAX_PRINCIPAL_ID_BYTES {
            return Err(ApprovalError::Invalid(format!(
                "principal id must be 1..={MAX_PRINCIPAL_ID_BYTES} bytes"
            )));
        }
        if !self
            .id
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err(ApprovalError::Invalid(
                "principal id must be printable ASCII".to_string(),
            ));
        }
        Ok(())
    }
}

/// The typed proposal an agent prepares for a human.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDraft {
    /// Consequential action class.
    pub kind: ApprovalKind,
    /// Stable subject (requirement id, supplier id, PO scope, ...).
    pub subject: String,
    /// Evidence references the human should review.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Exact proposed change in human-readable, bounded form.
    pub proposed_change: String,
    /// Risk class.
    pub risk: RiskLevel,
    /// Who proposes the action.
    pub requester: Principal,
    /// Revision the approval binds to (a set revision, record revision, ...).
    pub bound_revision: String,
    /// BLAKE3 digest of the exact proposed change and live subject state.
    pub bound_digest: String,
    /// Unix epoch milliseconds after which the approval is void.
    pub expires_at_ms: u64,
}

/// A durable approval request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    /// Deterministic request id (`apr-<32 hex>`).
    pub id: String,
    /// Consequential action class.
    pub kind: ApprovalKind,
    /// Stable subject.
    pub subject: String,
    /// Evidence references.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Proposed change.
    pub proposed_change: String,
    /// Risk.
    pub risk: RiskLevel,
    /// Requester.
    pub requester: Principal,
    /// Bound revision.
    pub bound_revision: String,
    /// Bound proposed-change digest.
    pub bound_digest: String,
    /// Creation time (epoch ms).
    pub created_at_ms: u64,
    /// Expiry (epoch ms).
    pub expires_at_ms: u64,
}

impl ApprovalRequest {
    /// Build a request with a deterministic, content-derived id.
    pub fn propose(draft: ApprovalDraft, now_ms: u64) -> Result<Self, ApprovalError> {
        if now_ms == 0 {
            return Err(ApprovalError::Invalid(
                "approval request time must be non-zero".to_string(),
            ));
        }
        if draft.expires_at_ms <= now_ms {
            return Err(ApprovalError::Invalid(
                "approval request expiry must be in the future".to_string(),
            ));
        }
        if draft.expires_at_ms - now_ms > MAX_REQUEST_LIFETIME_MS {
            return Err(ApprovalError::Invalid(format!(
                "approval request lifetime exceeds {MAX_REQUEST_LIFETIME_MS} ms"
            )));
        }
        // Approval text is untrusted model/CLI input that a human must
        // review: refuse credentials at the ingestion boundary instead of
        // persisting them in the durable hash-chained ledger.
        ensure_no_credential("subject", &draft.subject)?;
        ensure_no_credential("proposed change", &draft.proposed_change)?;
        ensure_no_credential("bound revision", &draft.bound_revision)?;
        ensure_no_credential("requester", &draft.requester.id)?;
        for evidence in &draft.evidence {
            ensure_no_credential("evidence", evidence)?;
        }
        let identity = RequestIdentity {
            kind: draft.kind,
            subject: &draft.subject,
            evidence: &draft.evidence,
            proposed_change: &draft.proposed_change,
            risk: draft.risk,
            requester: &draft.requester,
            bound_revision: &draft.bound_revision,
            bound_digest: &draft.bound_digest,
            created_at_ms: now_ms,
            expires_at_ms: draft.expires_at_ms,
        };
        let digest = content_digest(REQUEST_DOMAIN, &identity)?;
        let request = Self {
            id: format!("apr-{}", &digest[..32]),
            kind: draft.kind,
            subject: draft.subject,
            evidence: draft.evidence,
            proposed_change: draft.proposed_change,
            risk: draft.risk,
            requester: draft.requester,
            bound_revision: draft.bound_revision,
            bound_digest: draft.bound_digest,
            created_at_ms: now_ms,
            expires_at_ms: draft.expires_at_ms,
        };
        request.validate()?;
        Ok(request)
    }

    /// Validate bounds and shape.
    pub fn validate(&self) -> Result<(), ApprovalError> {
        if self.id.len() != 36
            || !self.id.starts_with("apr-")
            || !self.id[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ApprovalError::Invalid(
                "approval request id must be `apr-` plus 32 hex characters".to_string(),
            ));
        }
        bounded_single_line("subject", &self.subject, MAX_PRINCIPAL_ID_BYTES)?;
        bounded_text(
            "proposed_change",
            &self.proposed_change,
            MAX_APPROVAL_TEXT_BYTES,
        )?;
        bounded_single_line("bound_revision", &self.bound_revision, 256)?;
        if self.bound_digest.len() != 64
            || !self
                .bound_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ApprovalError::Invalid(
                "bound_digest must be 64 hex characters".to_string(),
            ));
        }
        if self.evidence.len() > MAX_APPROVAL_EVIDENCE {
            return Err(ApprovalError::Invalid(format!(
                "evidence exceeds {MAX_APPROVAL_EVIDENCE} entries"
            )));
        }
        let mut evidence_bytes = 0usize;
        for evidence in &self.evidence {
            bounded_single_line("evidence", evidence, MAX_APPROVAL_TEXT_BYTES)?;
            evidence_bytes = evidence_bytes.saturating_add(evidence.len());
        }
        if evidence_bytes > MAX_APPROVAL_EVIDENCE_BYTES {
            return Err(ApprovalError::Invalid(format!(
                "evidence exceeds {MAX_APPROVAL_EVIDENCE_BYTES} aggregate bytes"
            )));
        }
        ensure_no_credential("subject", &self.subject)?;
        ensure_no_credential("proposed change", &self.proposed_change)?;
        ensure_no_credential("bound revision", &self.bound_revision)?;
        ensure_no_credential("requester", &self.requester.id)?;
        for evidence in &self.evidence {
            ensure_no_credential("evidence", evidence)?;
        }
        self.requester.validate()?;
        if self.created_at_ms == 0
            || self.expires_at_ms <= self.created_at_ms
            || self.expires_at_ms - self.created_at_ms > MAX_REQUEST_LIFETIME_MS
        {
            return Err(ApprovalError::Invalid(
                "approval request has an invalid lifetime".to_string(),
            ));
        }
        Ok(())
    }

    /// The canonical request digest a decision binds to.
    pub fn digest(&self) -> Result<String, ApprovalError> {
        content_digest(REQUEST_DOMAIN, self)
    }

    /// The check a gate will make for this request.
    pub fn check(&self) -> ApprovalCheck {
        ApprovalCheck::new(
            self.kind,
            self.subject.clone(),
            self.bound_revision.clone(),
            self.bound_digest.clone(),
        )
    }
}

#[derive(Serialize)]
struct RequestIdentity<'a> {
    kind: ApprovalKind,
    subject: &'a str,
    evidence: &'a [String],
    proposed_change: &'a str,
    risk: RiskLevel,
    requester: &'a Principal,
    bound_revision: &'a str,
    bound_digest: &'a str,
    created_at_ms: u64,
    expires_at_ms: u64,
}

/// The exact gate check. Every gated action constructs this from live state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCheck {
    /// Required kind.
    pub kind: ApprovalKind,
    /// Required subject.
    pub subject: String,
    /// Required revision.
    pub bound_revision: String,
    /// Required proposed-change digest.
    pub bound_digest: String,
}

impl ApprovalCheck {
    /// Build a check.
    pub fn new(
        kind: ApprovalKind,
        subject: impl Into<String>,
        bound_revision: impl Into<String>,
        bound_digest: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            subject: subject.into(),
            bound_revision: bound_revision.into(),
            bound_digest: bound_digest.into(),
        }
    }
}

/// What the human decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum DecisionOutcome {
    /// The proposal may execute while the binding stays current.
    Approved,
    /// The proposal is refused; the action stays blocked.
    Denied {
        /// Bounded reason.
        reason: String,
    },
}

/// The Ed25519 attestation on a human decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionSignature {
    /// Exactly `ed25519`.
    pub algorithm: String,
    /// BLAKE3 fingerprint of the public key.
    pub key_id: String,
    /// Raw public key (lowercase hex).
    pub public_key: String,
    /// Signature over the domain-separated decision digest (lowercase hex).
    pub signature: String,
}

/// A durable, signed human decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecision {
    /// Request id.
    pub request_id: String,
    /// Request digest the decision binds to.
    pub request_digest: String,
    /// Approved or denied.
    pub outcome: DecisionOutcome,
    /// Human principal.
    pub principal: Principal,
    /// Bounded rationale.
    pub rationale: String,
    /// Decision time (epoch ms).
    pub decided_at_ms: u64,
    /// Ed25519 attestation.
    pub signature: DecisionSignature,
}

impl ApprovalDecision {
    /// The canonical decision statement digest (signature excluded).
    pub fn statement_digest(&self) -> Result<String, ApprovalError> {
        content_digest(
            DECISION_DOMAIN,
            &DecisionStatement {
                schema: "faktor.approval.decision/v1",
                request_id: &self.request_id,
                request_digest: &self.request_digest,
                outcome: &self.outcome,
                principal: &self.principal,
                rationale: &self.rationale,
                decided_at_ms: self.decided_at_ms,
            },
        )
    }

    /// Validate the decision's shape and bounds without trusting its
    /// signature. The signature is verified separately by [`Self::verify`].
    pub fn validate(&self) -> Result<(), ApprovalError> {
        if self.request_id.len() != 36
            || !self.request_id.starts_with("apr-")
            || !self.request_id[4..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ApprovalError::Invalid(
                "decision request id must be `apr-` plus 32 hex characters".to_string(),
            ));
        }
        if self.request_digest.len() != 64
            || !self
                .request_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ApprovalError::Invalid(
                "decision request digest must be 64 hex characters".to_string(),
            ));
        }
        if self.principal.kind != PrincipalKind::HumanOperator {
            return Err(ApprovalError::AgentApproval {
                principal: self.principal.id.clone(),
            });
        }
        self.principal.validate()?;
        bounded_text("rationale", &self.rationale, MAX_APPROVAL_TEXT_BYTES)?;
        ensure_no_credential("rationale", &self.rationale)?;
        match &self.outcome {
            DecisionOutcome::Approved => {}
            DecisionOutcome::Denied { reason } => {
                bounded_text("denial reason", reason, MAX_APPROVAL_TEXT_BYTES)?
            }
        }
        if self.decided_at_ms == 0 {
            return Err(ApprovalError::Invalid(
                "decision time must be non-zero".to_string(),
            ));
        }
        if self.signature.algorithm != "ed25519" {
            return Err(ApprovalError::Invalid(
                "decision signature algorithm must be ed25519".to_string(),
            ));
        }
        let public_key = parse_hex::<32>(&self.signature.public_key)?;
        let _signature = parse_hex::<64>(&self.signature.signature)?;
        if self.signature.key_id.len() != 64
            || !self
                .signature
                .key_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ApprovalError::Invalid(
                "decision key id must be 64 hex characters".to_string(),
            ));
        }
        if to_hex(blake3::hash(&public_key).as_bytes()) != self.signature.key_id {
            return Err(ApprovalError::SignatureMismatch(
                "decision key id does not fingerprint its public key".to_string(),
            ));
        }
        Ok(())
    }

    /// Verify the enclosed signature and, when `trusted` is non-empty, that
    /// the signing key is one of the registered human operators.
    pub fn verify(
        &self,
        trusted: Option<&std::collections::BTreeSet<String>>,
    ) -> Result<(), ApprovalError> {
        self.validate()?;
        if let Some(trusted) = trusted {
            if !trusted.contains(&self.signature.key_id) {
                return Err(ApprovalError::UntrustedOperator {
                    key_id: self.signature.key_id.clone(),
                });
            }
        }
        let public_key = parse_hex::<32>(&self.signature.public_key)?;
        let verifying_key = VerifyingKey::from_bytes(&public_key)
            .map_err(|error| ApprovalError::SignatureMismatch(error.to_string()))?;
        let signature_bytes = parse_hex::<64>(&self.signature.signature)?;
        let signature = Signature::from_bytes(&signature_bytes);
        let digest = self.statement_digest()?;
        verifying_key
            .verify(&signing_message(&digest), &signature)
            .map_err(|_| ApprovalError::SignatureMismatch("ed25519 verify failed".to_string()))
    }

    /// Attach the signature produced by an operator key.
    fn sign(mut self, key: &OperatorKeyHandle<'_>) -> Result<Self, ApprovalError> {
        let digest = self.statement_digest()?;
        let signature = key.key.sign(&signing_message(&digest));
        self.signature = DecisionSignature {
            algorithm: "ed25519".to_string(),
            key_id: key.key_id(),
            public_key: key.public_key_hex(),
            signature: to_hex(&signature.to_bytes()),
        };
        self.validate()?;
        Ok(self)
    }

    /// Build and sign a decision. Crate-internal: gates only accept a
    /// decision that the authority verified from its durable ledger.
    pub(crate) fn issue(
        request: &ApprovalRequest,
        outcome: DecisionOutcome,
        principal: Principal,
        rationale: String,
        decided_at_ms: u64,
        key: &OperatorKeyHandle<'_>,
    ) -> Result<Self, ApprovalError> {
        if principal.kind != PrincipalKind::HumanOperator {
            return Err(ApprovalError::AgentApproval {
                principal: principal.id.clone(),
            });
        }
        if principal.id == request.requester.id {
            return Err(ApprovalError::SelfApproval {
                principal: principal.id.clone(),
            });
        }
        if decided_at_ms < request.created_at_ms {
            return Err(ApprovalError::Invalid(
                "decision predates its request".to_string(),
            ));
        }
        if decided_at_ms > request.expires_at_ms {
            return Err(ApprovalError::Expired {
                request_id: request.id.clone(),
                expired_at_ms: request.expires_at_ms,
            });
        }
        ensure_no_credential("rationale", &rationale)?;
        if let DecisionOutcome::Denied { reason } = &outcome {
            ensure_no_credential("denial reason", reason)?;
        }
        let decision = Self {
            request_id: request.id.clone(),
            request_digest: request.digest()?,
            outcome,
            principal,
            rationale,
            decided_at_ms,
            signature: DecisionSignature {
                algorithm: String::new(),
                key_id: String::new(),
                public_key: String::new(),
                signature: String::new(),
            },
        };
        decision.sign(key)
    }
}

#[derive(Serialize)]
struct DecisionStatement<'a> {
    schema: &'static str,
    request_id: &'a str,
    request_digest: &'a str,
    outcome: &'a DecisionOutcome,
    principal: &'a Principal,
    rationale: &'a str,
    decided_at_ms: u64,
}

fn signing_message(digest: &str) -> Vec<u8> {
    let mut message = Vec::with_capacity(DECISION_SIGNATURE_DOMAIN.len() + digest.len());
    message.extend_from_slice(DECISION_SIGNATURE_DOMAIN);
    message.extend_from_slice(digest.as_bytes());
    message
}

/// An unforgeable, in-memory proof that a human approval is current and
/// bound to one exact revision/digest. Fields are private: only the
/// authority can mint a token, and no deserializer exists.
#[derive(Clone)]
pub struct ApprovalToken {
    kind: ApprovalKind,
    subject: String,
    bound_revision: String,
    bound_digest: String,
    request_id: String,
    decision_key_id: String,
    decided_at_ms: u64,
    expires_at_ms: u64,
    /// In-process monotonic deadline. Wall-clock changes cannot move it, so a
    /// token minted before a backwards clock jump cannot be revived.
    deadline: Instant,
}

impl fmt::Debug for ApprovalToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApprovalToken")
            .field("kind", &self.kind)
            .field("subject", &self.subject)
            .field("bound_revision", &self.bound_revision)
            .field("bound_digest", &self.bound_digest)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

impl ApprovalToken {
    pub(crate) fn from_verified(
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        now_ms: u64,
    ) -> Result<Self, ApprovalError> {
        if request.id != decision.request_id || request.digest()? != decision.request_digest {
            return Err(ApprovalError::Stale {
                request_id: request.id.clone(),
                expected_digest: request.bound_digest.clone(),
                actual_digest: decision.request_digest.clone(),
                expected_revision: request.bound_revision.clone(),
                actual_revision: request.bound_revision.clone(),
            });
        }
        Ok(Self {
            kind: request.kind,
            subject: request.subject.clone(),
            bound_revision: request.bound_revision.clone(),
            bound_digest: request.bound_digest.clone(),
            request_id: request.id.clone(),
            decision_key_id: decision.signature.key_id.clone(),
            decided_at_ms: decision.decided_at_ms,
            expires_at_ms: request.expires_at_ms,
            deadline: Instant::now()
                + Duration::from_millis(
                    request
                        .expires_at_ms
                        .saturating_sub(now_ms)
                        .min(MAX_REQUEST_LIFETIME_MS),
                ),
        })
    }

    /// Test-only token construction. Never compiled into production wiring:
    /// the `test-utils` feature is enabled exclusively by dev-dependencies.
    #[cfg(feature = "test-utils")]
    #[doc(hidden)]
    pub fn test_token(
        kind: ApprovalKind,
        subject: impl Into<String>,
        bound_revision: impl Into<String>,
        bound_digest: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            subject: subject.into(),
            bound_revision: bound_revision.into(),
            bound_digest: bound_digest.into(),
            request_id: "apr-test".to_string(),
            decision_key_id: "test".to_string(),
            decided_at_ms: 0,
            expires_at_ms: u64::MAX,
            deadline: Instant::now() + Duration::from_millis(MAX_REQUEST_LIFETIME_MS),
        }
    }

    /// The exact gate check this token satisfies.
    pub fn check(&self) -> ApprovalCheck {
        ApprovalCheck::new(
            self.kind,
            self.subject.clone(),
            self.bound_revision.clone(),
            self.bound_digest.clone(),
        )
    }

    /// Fail typed unless this token was issued for exactly `expected` and is
    /// still within the request lifetime. The authority also checks expiry
    /// when minting; this closes the cached-token window.
    pub fn require(&self, expected: &ApprovalCheck) -> Result<(), ApprovalError> {
        // The Instant deadline is authoritative while the process is alive;
        // the wall check is an additional fail-closed bound (a forward jump
        // or sleep expires early, a backward jump can never extend).
        if Instant::now() >= self.deadline || crate::authority::unix_now_ms() > self.expires_at_ms {
            return Err(ApprovalError::Expired {
                request_id: self.request_id.clone(),
                expired_at_ms: self.expires_at_ms,
            });
        }
        if self.kind == expected.kind
            && self.subject == expected.subject
            && self.bound_revision == expected.bound_revision
            && self.bound_digest == expected.bound_digest
        {
            return Ok(());
        }
        Err(ApprovalError::Stale {
            request_id: self.request_id.clone(),
            expected_digest: expected.bound_digest.clone(),
            actual_digest: self.bound_digest.clone(),
            expected_revision: expected.bound_revision.clone(),
            actual_revision: self.bound_revision.clone(),
        })
    }

    /// The action class.
    pub const fn kind(&self) -> ApprovalKind {
        self.kind
    }
    /// The subject.
    pub fn subject(&self) -> &str {
        &self.subject
    }
    /// The bound revision.
    pub fn bound_revision(&self) -> &str {
        &self.bound_revision
    }
    /// The bound proposed-change digest.
    pub fn bound_digest(&self) -> &str {
        &self.bound_digest
    }
    /// The human-approved request id.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    /// The human operator key fingerprint.
    pub fn decision_key_id(&self) -> &str {
        &self.decision_key_id
    }
    /// Decision time.
    pub const fn decided_at_ms(&self) -> u64 {
        self.decided_at_ms
    }
}

/// Typed approval refusal. `ApprovalRequired`, `Pending`, `Denied`, `Expired`
/// and `Stale` all block the action; none is ever coerced to allow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    /// No human approval exists for the exact action/revision/digest.
    #[error(
        "approval_required: {kind} on {subject} at revision {bound_revision} needs a human-operator approval bound to digest {bound_digest}"
    )]
    ApprovalRequired {
        /// Required kind.
        kind: ApprovalKind,
        /// Subject.
        subject: String,
        /// Revision.
        bound_revision: String,
        /// Digest.
        bound_digest: String,
    },
    /// A request exists but no human has decided it.
    #[error("approval_pending: {request_id} for {kind} on {subject} awaits a human operator")]
    Pending {
        /// Request id.
        request_id: String,
        /// Kind.
        kind: ApprovalKind,
        /// Subject.
        subject: String,
    },
    /// The bounded OPEN-request queue is full.
    #[error("approval_pending_limit: {open} open requests (max {max}); decide or let them expire before proposing more")]
    PendingLimit {
        /// Open requests observed under the ledger lock.
        open: usize,
        /// Configured bound.
        max: usize,
    },
    /// A human denied the request.
    #[error("approval_denied: {request_id} was denied by {principal}: {reason}")]
    Denied {
        /// Request id.
        request_id: String,
        /// Kind.
        kind: ApprovalKind,
        /// Subject.
        subject: String,
        /// Human principal.
        principal: String,
        /// Reason.
        reason: String,
    },
    /// The approval exists but is bound to a different revision/digest.
    #[error(
        "approval_stale: {request_id} is bound to revision {actual_revision}/digest {actual_digest}, but the action is now revision {expected_revision}/digest {expected_digest}"
    )]
    Stale {
        /// Request id.
        request_id: String,
        /// Current digest.
        expected_digest: String,
        /// Approved digest.
        actual_digest: String,
        /// Current revision.
        expected_revision: String,
        /// Approved revision.
        actual_revision: String,
    },
    /// The approval or request expired.
    #[error("approval_expired: {request_id} expired at {expired_at_ms}")]
    Expired {
        /// Request id.
        request_id: String,
        /// Expiry.
        expired_at_ms: u64,
    },
    /// The requester tried to approve its own request.
    #[error("approval_self: {principal} may not approve their own request")]
    SelfApproval {
        /// Principal.
        principal: String,
    },
    /// An agent or service attempted to decide.
    #[error("approval_agent: {principal} is not a human operator and cannot approve")]
    AgentApproval {
        /// Principal.
        principal: String,
    },
    /// The signing key is not a registered human-operator key.
    #[error("approval_untrusted: operator key {key_id} is not registered for the human principal")]
    UntrustedOperator {
        /// Key id.
        key_id: String,
    },
    /// The decision signature does not verify.
    #[error("approval_signature: {0}")]
    SignatureMismatch(String),
    /// A request/decision is malformed.
    #[error("approval_invalid: {0}")]
    Invalid(String),
    /// Durable ledger/registry failure (never treated as absence).
    #[error("approval_store: {0}")]
    Store(String),
    /// The independent monotonic ledger anchor is missing or unusable.
    #[error("approval_anchor_degraded: {reason} ({path})")]
    AnchorDegraded {
        /// Anchor path.
        path: String,
        /// Why the anchor cannot be trusted.
        reason: String,
    },
    /// The ledger is a valid prefix but has regressed behind the independent
    /// monotonic anchor.
    #[error("approval_ledger_rollback: anchor commits to entry {anchor_seq} but only {ledger_len} entries remain ({path})")]
    LedgerRollback {
        /// Anchor sequence.
        anchor_seq: u64,
        /// Observed ledger length.
        ledger_len: usize,
        /// Anchor path.
        path: String,
    },
    /// Another writer holds the ledger lock.
    #[error("approval_store_busy: {0}")]
    Busy(String),
    /// Exactly-once conflict.
    #[error("approval_conflict: {0}")]
    Conflict(String),
    /// Unknown request.
    #[error("approval_not_found: {0}")]
    NotFound(String),
}

impl ApprovalError {
    /// Stable JSON code for CLI/tool/closed-loop surfaces.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ApprovalRequired { .. } => "approval_required",
            Self::Pending { .. } => "approval_pending",
            Self::PendingLimit { .. } => "approval_pending_limit",
            Self::Denied { .. } => "approval_denied",
            Self::Stale { .. } => "approval_stale",
            Self::Expired { .. } => "approval_expired",
            Self::SelfApproval { .. } => "approval_self_refused",
            Self::AgentApproval { .. } => "approval_agent_refused",
            Self::UntrustedOperator { .. } => "approval_untrusted_operator",
            Self::SignatureMismatch(_) => "approval_signature_invalid",
            Self::Invalid(_) => "approval_invalid",
            Self::Store(_) => "approval_store_error",
            Self::AnchorDegraded { .. } => "approval_anchor_degraded",
            Self::LedgerRollback { .. } => "approval_ledger_rollback",
            Self::Busy(_) => "approval_store_busy",
            Self::Conflict(_) => "approval_conflict",
            Self::NotFound(_) => "approval_not_found",
        }
    }

    /// Typed JSON result. CLI and daemon tools return exactly this shape
    /// instead of prose when a consequential action is blocked.
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "status": self.code(),
            "code": self.code(),
            "message": self.to_string(),
        });
        let object = value.as_object_mut().expect("object literal");
        match self {
            Self::ApprovalRequired {
                kind,
                subject,
                bound_revision,
                bound_digest,
            } => {
                object.insert("kind".into(), kind.as_str().into());
                object.insert("subject".into(), subject.clone().into());
                object.insert("bound_revision".into(), bound_revision.clone().into());
                object.insert("bound_digest".into(), bound_digest.clone().into());
            }
            Self::PendingLimit { open, max } => {
                object.insert("open".into(), (*open).into());
                object.insert("max".into(), (*max).into());
            }
            Self::Pending {
                request_id,
                kind,
                subject,
            } => {
                object.insert("request_id".into(), request_id.clone().into());
                object.insert("kind".into(), kind.as_str().into());
                object.insert("subject".into(), subject.clone().into());
            }
            Self::Denied {
                request_id,
                kind,
                subject,
                principal,
                reason,
            } => {
                object.insert("request_id".into(), request_id.clone().into());
                object.insert("kind".into(), kind.as_str().into());
                object.insert("subject".into(), subject.clone().into());
                object.insert("principal".into(), principal.clone().into());
                object.insert("reason".into(), reason.clone().into());
            }
            Self::Stale {
                request_id,
                expected_digest,
                actual_digest,
                expected_revision,
                actual_revision,
            } => {
                object.insert("request_id".into(), request_id.clone().into());
                object.insert("expected_digest".into(), expected_digest.clone().into());
                object.insert("actual_digest".into(), actual_digest.clone().into());
                object.insert("expected_revision".into(), expected_revision.clone().into());
                object.insert("actual_revision".into(), actual_revision.clone().into());
            }
            Self::Expired {
                request_id,
                expired_at_ms,
            } => {
                object.insert("request_id".into(), request_id.clone().into());
                object.insert("expired_at_ms".into(), (*expired_at_ms).into());
            }
            Self::SelfApproval { principal } | Self::AgentApproval { principal } => {
                object.insert("principal".into(), principal.clone().into());
            }
            Self::UntrustedOperator { key_id } => {
                object.insert("key_id".into(), key_id.clone().into());
            }
            Self::AnchorDegraded { path, reason } => {
                object.insert("path".into(), path.clone().into());
                object.insert("reason".into(), reason.clone().into());
            }
            Self::LedgerRollback {
                anchor_seq,
                ledger_len,
                path,
            } => {
                object.insert("anchor_seq".into(), (*anchor_seq).into());
                object.insert("ledger_len".into(), (*ledger_len).into());
                object.insert("path".into(), path.clone().into());
            }
            _ => {}
        }
        value
    }
}

pub(crate) struct OperatorKeyHandle<'a> {
    pub key: &'a ed25519_dalek::SigningKey,
    pub public_key: [u8; 32],
}

impl OperatorKeyHandle<'_> {
    pub fn key_id(&self) -> String {
        to_hex(blake3::hash(&self.public_key).as_bytes())
    }

    pub fn public_key_hex(&self) -> String {
        to_hex(&self.public_key)
    }
}

/// Canonical BLAKE3 content digest over a domain-separated JSON value.
pub fn content_digest<T: Serialize>(domain: &[u8], value: &T) -> Result<String, ApprovalError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ApprovalError::Invalid(format!("cannot encode: {error}")))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}

fn bounded_text(field: &'static str, value: &str, max: usize) -> Result<(), ApprovalError> {
    if value.trim().is_empty() {
        return Err(ApprovalError::Invalid(format!("{field} is empty")));
    }
    if value.len() > max {
        return Err(ApprovalError::Invalid(format!(
            "{field} exceeds {max} bytes"
        )));
    }
    if contains_forbidden_formatting(value) {
        return Err(ApprovalError::Invalid(format!(
            "{field} contains control or bidirectional formatting characters"
        )));
    }
    Ok(())
}

/// Strict single-line variant for identifiers, subjects and evidence refs:
/// no newline/tab either, so a value can never forge a second line in a
/// human-review surface.
fn bounded_single_line(field: &'static str, value: &str, max: usize) -> Result<(), ApprovalError> {
    bounded_text(field, value, max)?;
    if value.chars().any(|c| matches!(c, '\n' | '\r' | '\t')) {
        return Err(ApprovalError::Invalid(format!(
            "{field} must be a single line"
        )));
    }
    Ok(())
}

/// C0/C1 controls (except the newline/tab allowed in prose) and Unicode
/// bidi overrides/isolates can spoof a human review surface.
fn contains_forbidden_formatting(value: &str) -> bool {
    value.chars().any(|c| {
        (c.is_control() && !matches!(c, '\n' | '\t'))
            || matches!(
                c,
                '\u{200E}'
                    | '\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}'
            )
    })
}

/// Refuse text that carries an obvious credential. The authoritative
/// scanner is shared with the daemon's outbound/tool-input gates; the error
/// names only the pattern class, never the matched secret.
fn ensure_no_credential(field: &'static str, value: &str) -> Result<(), ApprovalError> {
    let policy = faktor_security::SecretPolicy::default();
    let hits = faktor_security::scan_secrets(value, &policy);
    if let Some(hit) = hits.first() {
        return Err(ApprovalError::Invalid(format!(
            "{field} contains what looks like a credential ({}); remove it before recording the approval",
            hit.kind
        )));
    }
    Ok(())
}

fn parse_hex<const N: usize>(value: &str) -> Result<[u8; N], ApprovalError> {
    if value.len() != N * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApprovalError::Invalid(format!(
            "expected {} hex characters",
            N * 2
        )));
    }
    let mut out = [0u8; N];
    for (index, chunk) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let text = std::str::from_utf8(chunk)
            .map_err(|_| ApprovalError::Invalid("invalid hex".to_string()))?;
        out[index] = u8::from_str_radix(text, 16)
            .map_err(|_| ApprovalError::Invalid("invalid hex".to_string()))?;
    }
    Ok(out)
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_token_expires_on_its_instant_deadline_not_the_wall_clock() {
        // The persisted expiry is far in the future, so a wall-clock check
        // would accept this token; the Instant deadline is authoritative.
        let token = ApprovalToken {
            kind: ApprovalKind::SupplyPurchaseOrder,
            subject: "po-1".to_string(),
            bound_revision: "1".to_string(),
            bound_digest: "a".repeat(64),
            request_id: "apr-".to_string() + &"b".repeat(32),
            decision_key_id: "c".repeat(64),
            decided_at_ms: 1,
            expires_at_ms: u64::MAX,
            deadline: Instant::now() - Duration::from_secs(1),
        };
        let check = ApprovalCheck::new(
            ApprovalKind::SupplyPurchaseOrder,
            "po-1",
            "1",
            "a".repeat(64),
        );
        assert!(matches!(
            token.require(&check),
            Err(ApprovalError::Expired { .. })
        ));
    }
}
