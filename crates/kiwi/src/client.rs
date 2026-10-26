//! @authority-collapse: authority-bearing
//!
//! The bounded HTTP/JSON-wire Kiwi client.
//!
//! The client executes through the daemon's checked egress
//! [`HttpTransport`] — the SAME seam every provider adapter uses — with:
//!
//! - a configured provider watchdog (`timeout_ms`) and the caller's request
//!   deadline, whichever fires first (a caller-deadline expiry is terminal
//!   [`VerificationError::Timeout`]; the provider's OWN watchdog expiry is a
//!   recoverable [`VerificationError::Unavailable`] so dispatch may fail
//!   over);
//! - caller cancellation observed while the request is in flight;
//! - a bounded response body read ([`ResponseBudget`], the same
//!   budget-aware reader the semantic external provider uses) — an oversized
//!   body is refused typed BEFORE any allocation past the bound and before
//!   parsing;
//! - strict wire validation: schema version, op echo, job id echo, reply
//!   bounds, claim ids that must be obligations the submitted job actually
//!   carried, and required obligations that must all come back;
//! - a NEGOTIATED capability set: the adapter serves exactly the operations
//!   its explicit configuration (or the conservative default set) declares
//!   and refuses every other operation typed before any socket exists —
//!   capability truth is never a blanket claim;
//! - the bearer token is read through the [`SecretValue`] discipline at call
//!   time and the Authorization header is assembled without any
//!   whole-credential string formatting.
//!
//! An unconfigured host simply never constructs this provider
//! ([`KiwiProviderConfig`] is only built by the daemon when the section is
//! enabled); the verification registry then serves the native baseline.

use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_core::contract::{ProofBasis, ProofScope};
use faktor_provider::egress::{
    execute_post_json, BudgetComponent, EgressError, HttpTransport, ResponseBudget,
};
use faktor_security::secret::SecretValue;
use faktor_verify::{
    ArtifactRef, BoxFuture, ClaimVerificationResult, VerificationCapabilities, VerificationError,
    VerificationOp, VerificationProvider, VerificationProviderId, VerificationRequest,
    VerificationResponse,
};
use http::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::wire::{evidence_kind_for, KiwiJob, WireRequest, WireResponse, KIWI_SCHEMA_VERSION};

/// The canonical id of the external Kiwi verification provider.
pub const KIWI_PROVIDER_ID: &str = "kiwi";
/// Maximum bytes of a configured endpoint URL.
pub const MAX_KIWI_ENDPOINT_BYTES: usize = 2048;
/// Maximum bytes of a configured auth env var name.
pub const MAX_KIWI_AUTH_ENV_BYTES: usize = 128;
/// Maximum configured provider watchdog.
pub const MAX_KIWI_TIMEOUT_MS: u64 = 300_000;
/// Default configured provider watchdog.
pub const DEFAULT_KIWI_TIMEOUT_MS: u64 = 30_000;
/// Absolute hard ceiling on one wire response body (defense in depth).
pub const MAX_KIWI_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// The conservative DEFAULT negotiated operation set of the Kiwi adapter:
/// the operations an external independent verifier adds to the native
/// baseline (claim verification, contract/compatibility checks, generated
/// tests, security/performance/runtime checks). The native-owned operations
/// (build/lint/typecheck/tests) are available only when explicit
/// configuration adds them — capability truth is never a blanket claim.
pub const DEFAULT_KIWI_OPERATIONS: [VerificationOp; 7] = [
    VerificationOp::VerifyClaim,
    VerificationOp::RunGeneratedTests,
    VerificationOp::CheckContracts,
    VerificationOp::CheckCompatibility,
    VerificationOp::RunSecurityChecks,
    VerificationOp::RunPerformanceChecks,
    VerificationOp::RunRuntimeChecks,
];

/// Slack above the request output bound for framing/JSON overhead.
const WIRE_FRAME_SLACK_BYTES: usize = 64 * 1024;

fn default_timeout_ms() -> u64 {
    DEFAULT_KIWI_TIMEOUT_MS
}

/// Strict external-provider configuration (the daemon maps its
/// `[verification.kiwi]` section onto this): endpoint plus optional bearer
/// token by env-var NAME (the token value never rides config) plus the
/// explicitly NEGOTIATED operation set (absent = the conservative
/// [`DEFAULT_KIWI_OPERATIONS`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KiwiProviderConfig {
    pub endpoint: String,
    /// Name of the environment variable holding the bearer token; the value
    /// is read at call time and never stored.
    #[serde(default)]
    pub auth_env: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// The operations this deployment negotiates with the provider. Absent =
    /// [`DEFAULT_KIWI_OPERATIONS`]. Every operation outside the negotiated
    /// set is refused typed before any request is sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations: Option<Vec<VerificationOp>>,
}

impl KiwiProviderConfig {
    /// Typed validation: bounded endpoint, http(s) scheme, safe env-var name,
    /// bounded non-zero watchdog, and a non-empty, duplicate-free negotiated
    /// operation set. A hostile section is refused before any socket exists.
    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.endpoint.is_empty() || self.endpoint.len() > MAX_KIWI_ENDPOINT_BYTES {
            return Err(VerificationError::Malformed(format!(
                "kiwi endpoint must be 1..={MAX_KIWI_ENDPOINT_BYTES} bytes"
            )));
        }
        if !(self.endpoint.starts_with("http://") || self.endpoint.starts_with("https://")) {
            return Err(VerificationError::Malformed(format!(
                "kiwi endpoint {:?} must be an http(s) URL",
                self.endpoint
            )));
        }
        if let Some(var) = &self.auth_env {
            if var.is_empty()
                || var.len() > MAX_KIWI_AUTH_ENV_BYTES
                || !var.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(VerificationError::Malformed(format!(
                    "kiwi auth_env {var:?} must match [A-Za-z0-9_]+"
                )));
            }
        }
        if self.timeout_ms == 0 || self.timeout_ms > MAX_KIWI_TIMEOUT_MS {
            return Err(VerificationError::Malformed(format!(
                "kiwi timeout_ms must be 1..={MAX_KIWI_TIMEOUT_MS}"
            )));
        }
        if let Some(operations) = &self.operations {
            if operations.is_empty() {
                return Err(VerificationError::Malformed(
                    "kiwi operations must name at least one operation".to_string(),
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            for op in operations {
                if !seen.insert(*op) {
                    return Err(VerificationError::Malformed(format!(
                        "kiwi operations carry {} more than once",
                        op.as_str()
                    )));
                }
            }
        }
        Ok(())
    }

    /// The negotiated operation set of this configuration.
    pub fn negotiated_operations(&self) -> Vec<VerificationOp> {
        self.operations
            .clone()
            .unwrap_or_else(|| DEFAULT_KIWI_OPERATIONS.to_vec())
    }

    /// Build the provider over the daemon's checked egress transport.
    pub fn build(
        &self,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Arc<KiwiVerificationProvider>, VerificationError> {
        self.validate()?;
        let operations = self.negotiated_operations();
        let capabilities = VerificationCapabilities::of_many(&operations);
        Ok(Arc::new(KiwiVerificationProvider {
            id: VerificationProviderId::parse(KIWI_PROVIDER_ID)
                .expect("the static kiwi provider id is valid"),
            endpoint: self.endpoint.clone(),
            auth_env: self.auth_env.clone(),
            timeout: Duration::from_millis(self.timeout_ms),
            transport,
            max_response_bytes: MAX_KIWI_RESPONSE_BYTES,
            operations,
            capabilities,
        }))
    }
}

/// The external Kiwi verification provider (see the module docs).
pub struct KiwiVerificationProvider {
    id: VerificationProviderId,
    endpoint: String,
    auth_env: Option<String>,
    timeout: Duration,
    transport: Arc<dyn HttpTransport>,
    max_response_bytes: usize,
    /// The NEGOTIATED operation set (explicit config or the conservative
    /// default); every other operation is refused typed.
    operations: Vec<VerificationOp>,
    capabilities: VerificationCapabilities,
}

impl std::fmt::Debug for KiwiVerificationProvider {
    /// Redacted by construction: identity, endpoint, watchdog and the
    /// negotiated set only — never auth material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiwiVerificationProvider")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field("auth_env", &self.auth_env)
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("operations", &self.operations)
            .finish_non_exhaustive()
    }
}

impl KiwiVerificationProvider {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// The negotiated operation set (observability / tests).
    pub fn operations(&self) -> &[VerificationOp] {
        &self.operations
    }

    /// Identity of the transport this provider executes through (endpoint),
    /// used by hosts for health keying and diagnostics.
    pub fn transport_identity(&self) -> String {
        format!("http:{}", self.endpoint)
    }

    /// Submit one EXPLICIT job (the caller supplied objective, acceptance
    /// criteria and engineering contract). The request supplies identity,
    /// cancellation, deadline and the output bound; the job must respect the
    /// same output bound.
    pub fn submit(
        &self,
        request: VerificationRequest,
        job: KiwiJob,
    ) -> BoxFuture<'_, Result<VerificationResponse, VerificationError>> {
        Box::pin(async move { self.run_submission(request, job).await })
    }

    async fn run_submission(
        &self,
        request: VerificationRequest,
        job: KiwiJob,
    ) -> Result<VerificationResponse, VerificationError> {
        request.validate()?;
        job.validate()?;
        // Every production submission is claim-batch-only by construction:
        // an op-only job is equally neutral, so no job may carry contract
        // context (constraints/invariants/risks, including ephemeral
        // readiness) to the independent verifier.
        job.validate_claim_batch()?;
        if !self.capabilities.supports(request.op) {
            // Capability truth is the negotiated subset: an operation outside
            // it is refused typed before any request is ever framed.
            return Err(VerificationError::Unavailable(format!(
                "kiwi adapter does not serve operation {} (negotiated: {})",
                request.op.as_str(),
                self.operations
                    .iter()
                    .map(|op| op.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if job.op != request.op {
            return Err(VerificationError::Malformed(format!(
                "kiwi job op {} does not match the request op {}",
                job.op.as_str(),
                request.op.as_str()
            )));
        }
        if job.max_output_bytes != request.max_output_bytes as u64 {
            return Err(VerificationError::Malformed(
                "kiwi job max_output_bytes must equal the request max_output_bytes".to_string(),
            ));
        }
        if !request.claims.is_empty() {
            // Claim identity is structural: the job may carry exactly the
            // obligations the request named, nothing else.
            let requested: std::collections::BTreeSet<&str> = request
                .claims
                .iter()
                .map(|claim| claim.obligation_id.as_str())
                .collect();
            if job.obligation_ids() != requested {
                return Err(VerificationError::Malformed(
                    "kiwi job obligations must match the request claim ids exactly".to_string(),
                ));
            }
        }
        if request.cancellation.is_cancelled() {
            return Err(VerificationError::Timeout(
                "kiwi call cancelled before submission".to_string(),
            ));
        }
        let now_ms = faktor_verify::now_epoch_ms();
        let caller_remaining = match request.deadline_ms {
            Some(deadline_ms) => {
                if now_ms >= deadline_ms {
                    return Err(VerificationError::Timeout(
                        "kiwi request deadline expired before submission".to_string(),
                    ));
                }
                Some(Duration::from_millis((deadline_ms - now_ms) as u64))
            }
            None => None,
        };
        let caller_bounded = caller_remaining.is_some_and(|remaining| remaining <= self.timeout);
        let timeout = caller_remaining
            .map(|remaining| remaining.min(self.timeout))
            .unwrap_or(self.timeout);
        let headers = self.auth_headers()?;
        let value = serde_json::to_value(WireRequest {
            v: KIWI_SCHEMA_VERSION,
            op: request.op.as_str(),
            job: &job,
        })
        .map_err(|e| {
            VerificationError::Malformed(format!("kiwi request is not serializable: {e}"))
        })?;
        let max_body = (request.max_output_bytes)
            .saturating_add(WIRE_FRAME_SLACK_BYTES)
            .min(self.max_response_bytes);

        let started = Instant::now();
        let bytes = self
            .post(&value, headers, timeout, max_body, caller_bounded, &request)
            .await?;
        let frame: WireResponse = serde_json::from_slice(&bytes).map_err(|e| {
            VerificationError::Malformed(format!("malformed kiwi wire response: {e}"))
        })?;
        if frame.v != KIWI_SCHEMA_VERSION {
            return Err(VerificationError::Malformed(format!(
                "kiwi wire schema version {} is unsupported (this build speaks {KIWI_SCHEMA_VERSION})",
                frame.v
            )));
        }
        if frame.op != request.op.as_str() {
            return Err(VerificationError::UnknownOp(format!(
                "kiwi response op {:?} does not match the requested op {:?}",
                frame.op,
                request.op.as_str()
            )));
        }
        if frame.job_id != job.job_id {
            return Err(VerificationError::Malformed(format!(
                "kiwi response job_id {:?} does not match the submitted job {:?}",
                frame.job_id, job.job_id
            )));
        }
        frame.reply.validate()?;
        frame.reply.validate_claims_for(&job)?;

        let mut evidence: Vec<ArtifactRef> = Vec::new();
        let mut claim_results: Vec<ClaimVerificationResult> = Vec::new();
        for claim in &frame.reply.claims {
            evidence.extend(claim.evidence.iter().cloned());
            if let Some(counterexample) = &claim.counterexample {
                evidence.push(
                    ArtifactRef::new(
                        evidence_kind_for(job.op),
                        format!("kiwi:{}", claim.claim_id),
                        counterexample.clone(),
                    )
                    .map_err(|e| {
                        VerificationError::Malformed(format!(
                            "kiwi counterexample is not bounded: {e}"
                        ))
                    })?,
                );
            }
            if !request.claims.is_empty() {
                // The neutral per-claim result: the obligation id is the SAME
                // id Faktor submitted (never a Kiwi-private syntax), and the
                // declared strength is validated by the neutral import.
                let result = ClaimVerificationResult {
                    obligation_id: claim.claim_id.clone(),
                    subject_snapshot: request.subject_snapshot.clone(),
                    state: claim.state,
                    evidence_strength: claim.effective_strength(),
                    evidence: claim.evidence.clone(),
                    counterexample: claim.counterexample.iter().cloned().collect(),
                    provider: self.id.clone(),
                    basis: ProofBasis {
                        subject: request.subject_snapshot.clone(),
                        scope: ProofScope::SnapshotBound,
                    },
                };
                result.validate()?;
                claim_results.push(result);
            }
        }
        evidence.extend(frame.reply.evidence.iter().cloned());

        let mut response =
            VerificationResponse::new(self.id.clone(), request.op, frame.reply.outcome)
                .with_summary(frame.reply.summary)
                .with_evidence(evidence)
                .with_claims(claim_results)
                .with_duration_ms(started.elapsed().as_millis() as u64);
        response.fit_output(request.max_output_bytes);
        response.validate(&request)?;
        Ok(response)
    }

    fn auth_headers(&self) -> Result<HeaderMap, VerificationError> {
        let mut headers = HeaderMap::new();
        if let Some(var) = &self.auth_env {
            // The SecretValue discipline: the env value is moved straight
            // into the zeroizing wrapper, the Authorization header is
            // assembled over ONE zeroizing buffer, and no whole-credential
            // string is ever formatted.
            let secret = SecretValue::new(std::env::var(var).map_err(|_| {
                VerificationError::Unavailable(format!("kiwi auth env {var} is not set"))
            })?);
            let mut value_bytes = Zeroizing::new(Vec::with_capacity(7 + secret.len()));
            value_bytes.extend_from_slice(b"Bearer ");
            value_bytes.extend_from_slice(secret.expose().as_bytes());
            let value = HeaderValue::from_bytes(&value_bytes).map_err(|_| {
                VerificationError::Malformed(format!(
                    "kiwi auth env {var} is not a valid header value"
                ))
            })?;
            headers.insert(AUTHORIZATION, value);
        }
        Ok(headers)
    }

    /// One bounded POST + streaming read with cancellation and the effective
    /// timeout. `caller_bounded` records WHICH bound the timeout is: the
    /// caller's deadline (terminal) or the provider watchdog (recoverable).
    async fn post(
        &self,
        value: &serde_json::Value,
        headers: HeaderMap,
        timeout: Duration,
        max_body: usize,
        caller_bounded: bool,
        request: &VerificationRequest,
    ) -> Result<Vec<u8>, VerificationError> {
        let send = async {
            let response =
                execute_post_json(self.transport.as_ref(), &self.endpoint, headers, value)
                    .await
                    .map_err(|e| {
                        VerificationError::Unavailable(format!("kiwi egress refused: {e}"))
                    })?;
            let status = response.status();
            if !status.is_success() {
                return Err(match status.as_u16() {
                    401 | 403 => {
                        VerificationError::Refused(format!("kiwi endpoint answered HTTP {status}"))
                    }
                    400..=499 => VerificationError::Malformed(format!(
                        "kiwi endpoint answered HTTP {status}"
                    )),
                    _ => VerificationError::Unavailable(format!(
                        "kiwi endpoint answered HTTP {status}"
                    )),
                });
            }
            let budget = ResponseBudget::for_timeout(timeout, max_body as u64);
            let mut body = response.into_budgeted(budget);
            let mut bytes: Vec<u8> = Vec::new();
            loop {
                match body.next_chunk().await {
                    Ok(Some(chunk)) => {
                        if bytes.len().saturating_add(chunk.len()) > max_body {
                            return Err(VerificationError::OutputTooLarge {
                                max: max_body,
                                actual: bytes.len().saturating_add(chunk.len()),
                            });
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(EgressError::ResponseBudgetExceeded {
                        component: BudgetComponent::Bytes,
                        ..
                    }) => {
                        return Err(VerificationError::OutputTooLarge {
                            max: max_body,
                            actual: max_body.saturating_add(1),
                        });
                    }
                    Err(e) => {
                        return Err(VerificationError::Unavailable(format!(
                            "kiwi response read failed: {e}"
                        )));
                    }
                }
            }
            Ok::<Vec<u8>, VerificationError>(bytes)
        };
        tokio::select! {
            _ = request.cancellation.cancelled() => {
                Err(VerificationError::Timeout(
                    "kiwi call cancelled by the caller".to_string(),
                ))
            }
            result = tokio::time::timeout(timeout, send) => match result {
                Ok(result) => result,
                Err(_) => {
                    if caller_bounded {
                        Err(VerificationError::Timeout(
                            "kiwi request deadline expired while the call was in flight".to_string(),
                        ))
                    } else {
                        Err(VerificationError::Unavailable(format!(
                            "kiwi provider watchdog expired after {}ms; the call is recoverable and dispatch may fail over",
                            timeout.as_millis()
                        )))
                    }
                }
            },
        }
    }
}

impl VerificationProvider for KiwiVerificationProvider {
    fn id(&self) -> VerificationProviderId {
        self.id.clone()
    }

    fn capabilities(&self) -> VerificationCapabilities {
        self.capabilities
    }

    fn verify(
        &self,
        request: VerificationRequest,
    ) -> BoxFuture<'_, Result<VerificationResponse, VerificationError>> {
        if !self.capabilities.supports(request.op) {
            // Refused typed, before any job is framed: the negotiated subset
            // is the adapter's capability truth.
            let message = format!(
                "kiwi adapter does not serve operation {} (negotiated: {})",
                request.op.as_str(),
                self.operations
                    .iter()
                    .map(|op| op.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Box::pin(async move { Err(VerificationError::Unavailable(message)) });
        }
        let job = if request.claims.is_empty() {
            KiwiJob::for_request(&request)
        } else {
            KiwiJob::for_claims(&request)
        };
        match job {
            Ok(job) => self.submit(request, job),
            Err(err) => Box::pin(async move { Err(err) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> KiwiProviderConfig {
        KiwiProviderConfig {
            endpoint: "http://127.0.0.1:1/verify".to_string(),
            auth_env: None,
            timeout_ms: 1_000,
            operations: None,
        }
    }

    fn test_transport() -> Arc<dyn HttpTransport> {
        Arc::new(
            faktor_provider::egress::PolicyCheckedHttpTransport::with_policy_for_tests(
                faktor_security::destination::DestinationPolicy::parse_lines([
                    "http://127.0.0.1:1",
                ])
                .unwrap(),
            ),
        )
    }

    #[test]
    fn hostile_configs_are_refused_typed() {
        let base = base_config();
        base.validate().unwrap();
        for endpoint in ["", "not-a-url", "ftp://example", "file:///etc/passwd"] {
            let cfg = KiwiProviderConfig {
                endpoint: endpoint.to_string(),
                ..base.clone()
            };
            assert!(cfg.validate().is_err(), "{endpoint:?} must be refused");
        }
        let cfg = KiwiProviderConfig {
            endpoint: "x".repeat(MAX_KIWI_ENDPOINT_BYTES + 1),
            ..base.clone()
        };
        assert!(cfg.validate().is_err());
        for auth_env in ["", "has space", "semi;colon", "lower\ncase"] {
            let cfg = KiwiProviderConfig {
                auth_env: Some(auth_env.to_string()),
                ..base.clone()
            };
            assert!(cfg.validate().is_err(), "{auth_env:?} must be refused");
        }
        for timeout_ms in [0, MAX_KIWI_TIMEOUT_MS + 1] {
            let cfg = KiwiProviderConfig {
                timeout_ms,
                ..base.clone()
            };
            assert!(cfg.validate().is_err(), "{timeout_ms} must be refused");
        }
        // A hostile negotiation is refused: empty or duplicate operations.
        let cfg = KiwiProviderConfig {
            operations: Some(Vec::new()),
            ..base.clone()
        };
        assert!(cfg.validate().is_err());
        let cfg = KiwiProviderConfig {
            operations: Some(vec![
                VerificationOp::VerifyClaim,
                VerificationOp::VerifyClaim,
            ]),
            ..base
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn capability_truth_is_the_negotiated_subset_never_a_blanket_claim() {
        // The absent-operations default is the conservative explicit set,
        // which does NOT include the native-owned operations.
        let cfg = base_config();
        let operations = cfg.negotiated_operations();
        assert_eq!(operations, DEFAULT_KIWI_OPERATIONS.to_vec());
        assert!(!operations.contains(&VerificationOp::RunTests));
        assert!(!operations.contains(&VerificationOp::Build));
        assert!(operations.contains(&VerificationOp::VerifyClaim));
        let built = cfg.build(test_transport()).unwrap();
        let capabilities = VerificationProvider::capabilities(built.as_ref());
        assert!(capabilities.supports(VerificationOp::VerifyClaim));
        assert!(!capabilities.supports(VerificationOp::RunTests));
        assert!(!capabilities.supports(VerificationOp::Build));

        // An explicit negotiation both narrows and extends, and the adapter
        // DECLARES exactly that set.
        let cfg = KiwiProviderConfig {
            operations: Some(vec![VerificationOp::RunTests, VerificationOp::Lint]),
            ..base_config()
        };
        let built = cfg.build(test_transport()).unwrap();
        let capabilities = VerificationProvider::capabilities(built.as_ref());
        assert!(capabilities.supports(VerificationOp::RunTests));
        assert!(capabilities.supports(VerificationOp::Lint));
        assert!(!capabilities.supports(VerificationOp::VerifyClaim));
        assert_eq!(
            built.operations(),
            [VerificationOp::RunTests, VerificationOp::Lint]
        );
    }

    #[tokio::test]
    async fn an_unsupported_operation_is_refused_typed_before_any_request() {
        let built = base_config().build(test_transport()).unwrap();
        let request = VerificationRequest::new(
            faktor_core::OpId::new(1),
            faktor_core::SessionId::new(1),
            faktor_core::WorkspaceId::new(1),
            faktor_verify::now_epoch_ms(),
            faktor_core::CancellationToken::new(),
            VerificationOp::RunTests,
        );
        let err = built
            .verify(request)
            .await
            .expect_err("RunTests is outside the negotiated default set");
        match err {
            VerificationError::Unavailable(detail) => {
                assert!(detail.contains("run_tests"), "{detail}");
                assert!(detail.contains("negotiated"), "{detail}");
            }
            other => panic!("expected typed Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn bearer_token_never_leaks_into_debug_or_errors() {
        const PLANTED: &str = "PLANTED-KIWI-TOKEN-do-not-leak-0123456789";
        const VAR: &str = "FAKTOR_KIWI_CLIENT_TEST_TOKEN_TRAP";
        std::env::set_var(VAR, PLANTED);
        let cfg = KiwiProviderConfig {
            auth_env: Some(VAR.to_string()),
            ..base_config()
        };
        let built = cfg.build(test_transport()).unwrap();
        let debug = format!("{built:?}");
        assert!(!debug.contains(PLANTED), "provider Debug leaked: {debug}");
        // The header carries exactly the bearer credential...
        let headers = built.auth_headers().unwrap();
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            format!("Bearer {PLANTED}")
        );
        // ...while a refusal never carries the value, only the env NAME.
        std::env::set_var(VAR, "bad\nvalue");
        let err = built.auth_headers().unwrap_err();
        let rendered = format!("{err} {err:?}");
        assert!(!rendered.contains(PLANTED), "error leaked: {rendered}");
        assert!(!rendered.contains("bad"), "error leaked bytes: {rendered}");
        assert!(rendered.contains(VAR), "{rendered}");
        std::env::remove_var(VAR);
        // A missing env is a recoverable unavailable naming only the var.
        let err = built.auth_headers().unwrap_err();
        let rendered = format!("{err} {err:?}");
        assert!(rendered.contains(VAR), "{rendered}");
    }
}
