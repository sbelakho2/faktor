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
//!   bounds, and claim ids that must be obligations the submitted job
//!   actually carried.
//!
//! An unconfigured host simply never constructs this provider
//! ([`KiwiProviderConfig`] is only built by the daemon when the section is
//! enabled); the verification registry then serves the native baseline.

use std::sync::Arc;
use std::time::{Duration, Instant};

use faktor_provider::egress::{
    execute_post_json, BudgetComponent, EgressError, HttpTransport, ResponseBudget,
};
use faktor_verify::{
    ArtifactRef, BoxFuture, VerificationCapabilities, VerificationError, VerificationProvider,
    VerificationProviderId, VerificationRequest, VerificationResponse,
};
use http::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};

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

/// Slack above the request output bound for framing/JSON overhead.
const WIRE_FRAME_SLACK_BYTES: usize = 64 * 1024;

fn default_timeout_ms() -> u64 {
    DEFAULT_KIWI_TIMEOUT_MS
}

/// Strict external-provider configuration (the daemon maps its
/// `[verification.kiwi]` section onto this): endpoint plus optional bearer
/// token by env-var NAME (the token value never rides config).
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
}

impl KiwiProviderConfig {
    /// Typed validation: bounded endpoint, http(s) scheme, safe env-var name,
    /// bounded non-zero watchdog. A hostile section is refused before any
    /// socket exists.
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
        Ok(())
    }

    /// Build the provider over the daemon's checked egress transport.
    pub fn build(
        &self,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Arc<KiwiVerificationProvider>, VerificationError> {
        self.validate()?;
        Ok(Arc::new(KiwiVerificationProvider {
            id: VerificationProviderId::parse(KIWI_PROVIDER_ID)
                .expect("the static kiwi provider id is valid"),
            endpoint: self.endpoint.clone(),
            auth_env: self.auth_env.clone(),
            timeout: Duration::from_millis(self.timeout_ms),
            transport,
            max_response_bytes: MAX_KIWI_RESPONSE_BYTES,
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
}

impl KiwiVerificationProvider {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
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
        }
        evidence.extend(frame.reply.evidence.iter().cloned());

        let mut response =
            VerificationResponse::new(self.id.clone(), request.op, frame.reply.outcome)
                .with_summary(frame.reply.summary)
                .with_evidence(evidence)
                .with_duration_ms(started.elapsed().as_millis() as u64);
        response.fit_output(request.max_output_bytes);
        response.validate(&request)?;
        Ok(response)
    }

    fn auth_headers(&self) -> Result<HeaderMap, VerificationError> {
        let mut headers = HeaderMap::new();
        if let Some(var) = &self.auth_env {
            let secret = std::env::var(var).map_err(|_| {
                VerificationError::Unavailable(format!("kiwi auth env {var} is not set"))
            })?;
            let value = HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
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
        VerificationCapabilities::ALL
    }

    fn verify(
        &self,
        request: VerificationRequest,
    ) -> BoxFuture<'_, Result<VerificationResponse, VerificationError>> {
        let job = KiwiJob::for_request(&request);
        match job {
            Ok(job) => self.submit(request, job),
            Err(err) => Box::pin(async move { Err(err) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_configs_are_refused_typed() {
        let base = KiwiProviderConfig {
            endpoint: "http://127.0.0.1:1/verify".to_string(),
            auth_env: None,
            timeout_ms: 1_000,
        };
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
    }
}
