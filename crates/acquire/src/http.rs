//! Bounded direct HTTP acquisition through the injected transport.
//!
//! This is the `DirectHttp` mechanism of `docs/acquire.md` §1.3: a stable
//! first-party JSON/HTTP response, fetched through the checked egress
//! transport the daemon injects. This crate never builds an HTTP client and
//! never executes a raw request itself — it builds a
//! [`faktor_provider::egress::RawRequest`] and hands it to the transport,
//! which owns destination policy, secret scanning and the absolute
//! materialization cap.
//!
//! Every dimension is bounded and every bound has a typed error:
//!
//! | bound                | error                                  |
//! |----------------------|----------------------------------------|
//! | response bytes       | [`AcquisitionError::ResponseTooLarge`] |
//! | JSON nesting         | [`AcquisitionError::NestingTooDeep`]   |
//! | redirect hops        | [`AcquisitionError::TooManyRedirects`] |
//! | request time         | [`AcquisitionError::NetworkTimeout`] / `Deadline` |
//! | URL length           | [`AcquisitionError::InvalidRequest`]   |
//!
//! A `304 Not Modified` short-circuits *before any parsing*: the caller keeps
//! its stored snapshot (no parse, no duplicate snapshot) and only learns that
//! the copy is still current. `store_acquisition` encodes exactly that.

use std::time::Duration;

use faktor_provider::egress::{execute_raw, RawRequest, RawResponse, ResponseBudget};
use serde::{Deserialize, Serialize};

use crate::cache::{CacheEntry, CacheKey, ConditionalValidators, FieldFreshnessClass};
use crate::ctx::AcquireCtx;
use crate::error::{invalid, AcquisitionError};
use crate::mechanism::AcquisitionMechanism;
use crate::provenance::AcquisitionProvenance;
use crate::quota::parse_retry_after_seconds;

/// The bounds of the direct-HTTP mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectHttpPolicy {
    /// Maximum accepted response body (the transport additionally caps
    /// materialization at its own absolute limit).
    pub max_response_bytes: u64,
    /// Maximum accepted JSON nesting depth.
    pub max_json_nesting: u32,
    /// Maximum redirect hops.
    pub max_redirects: u32,
    /// Per-request time bound.
    pub request_timeout_ms: u64,
    /// Maximum URL length.
    pub max_url_bytes: usize,
    /// Default wait when a `Retry-After` header is missing or unparseable.
    pub default_retry_after_ms: u64,
    /// Hard cap on a honored `Retry-After`.
    pub max_retry_after_ms: u64,
}

impl Default for DirectHttpPolicy {
    fn default() -> Self {
        Self {
            max_response_bytes: 2 * 1024 * 1024,
            max_json_nesting: 32,
            max_redirects: 3,
            request_timeout_ms: 15_000,
            max_url_bytes: 4096,
            default_retry_after_ms: 1_000,
            max_retry_after_ms: 300_000,
        }
    }
}

/// One direct HTTP fetch request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpFetch {
    /// The absolute http(s) URL.
    pub url: String,
    /// Extra request headers (never credentials; the transport scans them).
    pub headers: Vec<(String, String)>,
    /// Conditional validators, when a stored copy exists.
    pub validators: Option<ConditionalValidators>,
}

impl HttpFetch {
    /// A GET for `url`.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            validators: None,
        }
    }

    /// Add a request header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Attach conditional validators.
    pub fn with_validators(mut self, validators: ConditionalValidators) -> Self {
        self.validators = Some(validators);
        self
    }
}

/// One bounded acquisition outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpAcquisition {
    /// The final HTTP status.
    pub status: u16,
    /// Whether the response was `304 Not Modified`.
    pub not_modified: bool,
    /// The parsed JSON body (`None` on 304 and on non-JSON bodies).
    pub json: Option<serde_json::Value>,
    /// The body size in bytes.
    pub body_bytes: u64,
    /// How many redirects were followed.
    pub redirects: u32,
    /// The final URL after redirects.
    pub final_url: String,
    /// Validators the response advertised (empty on 304).
    pub validators: ConditionalValidators,
    /// Provenance of the observed content (`None` on 304: nothing new was
    /// observed, the stored provenance stays authoritative).
    pub provenance: Option<AcquisitionProvenance>,
}

/// Fetch one URL through the injected transport under every bound.
pub async fn fetch_direct_http(
    ctx: &AcquireCtx,
    fetch: &HttpFetch,
) -> Result<HttpAcquisition, AcquisitionError> {
    ctx.check_active()?;
    let policy = *ctx.policy();
    let observed_at_ms = ctx.now_ms();
    // Quota and Retry-After gates are checked before any transport call, so a
    // refused attempt never leaves the process.
    ctx.try_acquire_quota()?;
    validate_fetch_url(&fetch.url, policy.max_url_bytes)?;

    let conditional = fetch
        .validators
        .clone()
        .filter(|validators| !validators.is_empty());
    let mut url = fetch.url.clone();
    let mut redirects = 0u32;

    loop {
        ctx.check_active()?;
        let headers = merged_headers(&fetch.headers, conditional.as_ref());
        let mut raw = RawRequest::new("GET", url.clone());
        for (name, value) in &headers {
            raw = raw.header(name, value.clone());
        }
        let response = execute_guarded(ctx, &policy, raw).await?;
        let status = response.status;

        match status {
            // 304: no parse, no duplicate snapshot. Nothing new was observed,
            // so there is no new provenance either.
            304 => {
                ctx.report_success();
                let mut validators = ConditionalValidators::from_headers(
                    response
                        .headers
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str())),
                );
                if validators.is_empty() {
                    validators = conditional.clone().unwrap_or_default();
                }
                return Ok(HttpAcquisition {
                    status,
                    not_modified: true,
                    json: None,
                    body_bytes: 0,
                    redirects,
                    final_url: url,
                    validators,
                    provenance: None,
                });
            }
            300..=399 => {
                let Some(location) = response.header("location") else {
                    return Err(invalid("redirect response without a Location header"));
                };
                if redirects >= policy.max_redirects {
                    return Err(AcquisitionError::TooManyRedirects {
                        limit: policy.max_redirects,
                    });
                }
                url = resolve_redirect(&url, location, policy.max_url_bytes)?;
                redirects = redirects.saturating_add(1);
            }
            200..=299 => {
                let body_bytes = response.body.len() as u64;
                if body_bytes > policy.max_response_bytes {
                    return Err(AcquisitionError::ResponseTooLarge {
                        limit_bytes: policy.max_response_bytes,
                    });
                }
                let json = parse_bounded_json(&response.body, policy.max_json_nesting)?;
                ctx.report_success();
                let validators = ConditionalValidators::from_headers(
                    response
                        .headers
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str())),
                );
                let provenance = AcquisitionProvenance::observed(
                    AcquisitionMechanism::DirectHttp,
                    observed_at_ms,
                    &response.body,
                    conditional.is_some(),
                    Some(url.clone()),
                );
                return Ok(HttpAcquisition {
                    status,
                    not_modified: false,
                    json: Some(json),
                    body_bytes,
                    redirects,
                    final_url: url,
                    validators,
                    provenance: Some(provenance),
                });
            }
            401 | 407 => {
                let err = AcquisitionError::AuthenticationRequired;
                ctx.report_error(&err);
                return Err(err);
            }
            403 => {
                let err = AcquisitionError::VerificationRequired {
                    kind: crate::error::VerificationKind::Challenge,
                };
                ctx.report_error(&err);
                return Err(err);
            }
            404 | 410 => return Err(AcquisitionError::NotFound),
            408 | 504 => {
                let err = AcquisitionError::NetworkTimeout;
                ctx.report_error(&err);
                return Err(err);
            }
            429 => {
                let retry_after_ms = response
                    .header("retry-after")
                    .and_then(parse_retry_after_seconds)
                    .unwrap_or(policy.default_retry_after_ms)
                    .min(policy.max_retry_after_ms);
                ctx.record_retry_after(retry_after_ms);
                let err = AcquisitionError::RateLimited { retry_after_ms };
                ctx.report_error(&err);
                return Err(err);
            }
            500..=599 => {
                let err = AcquisitionError::ApiUnavailable;
                ctx.report_error(&err);
                ctx.report_transient_throttle();
                return Err(err);
            }
            other => return Err(invalid(format!("unexpected response status {other}"))),
        }
    }
}

/// Execute one request with cancellation, deadline and request-time guards.
async fn execute_guarded(
    ctx: &AcquireCtx,
    policy: &DirectHttpPolicy,
    raw: RawRequest,
) -> Result<RawResponse, AcquisitionError> {
    let budget_ms = match ctx.remaining_ms() {
        Some(remaining) => policy.request_timeout_ms.min(remaining.max(1)),
        None => policy.request_timeout_ms,
    };
    // The REQUIRED response budget: head/idle/total all equal this
    // attempt's wall bound and the byte cap is the source policy's response
    // bound (itself under the seam's materialization cap).
    let budget = ResponseBudget::for_timeout(
        Duration::from_millis(budget_ms.max(1)),
        policy.max_response_bytes,
    );
    let request = execute_raw(ctx.transport().as_ref(), raw, &budget);
    tokio::pin!(request);
    let timed = tokio::time::timeout(Duration::from_millis(budget_ms.max(1)), request);
    tokio::pin!(timed);
    tokio::select! {
        biased;
        _ = ctx.cancellation().cancelled() => Err(AcquisitionError::Cancelled),
        outcome = &mut timed => match outcome {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(egress)) => {
                let err = AcquisitionError::from(egress);
                if err.is_transient() {
                    ctx.report_error(&err);
                    ctx.report_transient_throttle();
                }
                Err(err)
            }
            Err(_) => {
                // Distinguish "the caller's deadline fired" from "the request
                // itself timed out".
                let err = match ctx.check_active() {
                    Err(active) => active,
                    Ok(()) => AcquisitionError::NetworkTimeout,
                };
                ctx.report_error(&err);
                if err.is_transient() {
                    ctx.report_transient_throttle();
                }
                Err(err)
            }
        }
    }
}

fn merged_headers(
    base: &[(String, String)],
    validators: Option<&ConditionalValidators>,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = base.to_vec();
    if let Some(validators) = validators {
        for (name, value) in validators.headers() {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
            headers.push((name, value));
        }
    }
    headers
}

/// Validate one absolute http(s) URL under the length bound.
///
/// The scheme/host/port/userinfo semantics are DELEGATED to the shared
/// parser the provider transport's destination gate uses
/// (`faktor_provider::egress::validate_provider_base_url`): acquire never
/// re-derives URL semantics from strings, so an accepted URL can never
/// parse to a different origin at the hand-built
/// [`faktor_provider::egress::RawRequest`] than the one validated here
/// (backslash/authority normalization, IDN and port canonicalization all
/// happen inside that single parser). A rejected URL surfaces a typed
/// refusal WITHOUT echoing the raw text (it may carry a planted secret).
pub fn validate_fetch_url(url: &str, max_url_bytes: usize) -> Result<(), AcquisitionError> {
    if url.is_empty() {
        return Err(invalid("url is empty"));
    }
    if url.len() > max_url_bytes {
        return Err(invalid(format!("url exceeds {max_url_bytes} bytes")));
    }
    if url.chars().any(char::is_control) {
        return Err(invalid("url contains control characters"));
    }
    faktor_provider::egress::validate_provider_base_url(url).map_err(|_| {
        invalid(
            "url is not a fetchable absolute http(s) URL (shared parser refused: scheme/host/\
             userinfo/port must be valid)",
        )
    })?;
    Ok(())
}

/// Resolve a redirect target against the current URL.
///
/// The resolution itself is DELEGATED to the shared URL parser
/// (`Url::join`, via the provider's strict http(s) validator), so the
/// returned string is the parser's own canonical form: whatever the policy
/// gate later parses from the hand-built
/// [`faktor_provider::egress::RawRequest`] is byte-identical to what was
/// validated here. A target that does not resolve to an absolute http(s)
/// URL inside the length bound (non-HTTP scheme, embedded credentials,
/// control characters) is a typed refusal.
pub fn resolve_redirect(
    base: &str,
    location: &str,
    max_url_bytes: usize,
) -> Result<String, AcquisitionError> {
    let location = location.trim();
    if location.is_empty() {
        return Err(invalid("redirect Location is empty"));
    }
    if location.len() > max_url_bytes {
        return Err(invalid(format!(
            "redirect target exceeds {max_url_bytes} bytes"
        )));
    }
    if location.chars().any(char::is_control) {
        return Err(invalid("redirect target contains control characters"));
    }
    let base = faktor_provider::egress::validate_provider_base_url(base)
        .map_err(|_| invalid("base url is not a fetchable absolute http(s) URL"))?;
    let resolved = base
        .join(location)
        .map_err(|_| invalid("redirect Location does not resolve against the base URL"))?;
    let resolved = resolved.to_string();
    if resolved.len() > max_url_bytes {
        return Err(invalid(format!(
            "redirect target exceeds {max_url_bytes} bytes"
        )));
    }
    validate_fetch_url(&resolved, max_url_bytes)?;
    Ok(resolved)
}

/// Scan a JSON document for nesting depth *before* parsing. The scan is
/// string-aware (brackets inside string literals do not count) and stops at
/// the bound with a typed error, so a hostile deeply-nested body is never
/// handed to the parser.
pub fn check_json_nesting(bytes: &[u8], max_nesting: u32) -> Result<u32, AcquisitionError> {
    let mut depth = 0u32;
    let mut max_depth = 0u32;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth.saturating_add(1);
                max_depth = max_depth.max(depth);
                if depth > max_nesting {
                    return Err(AcquisitionError::NestingTooDeep {
                        limit: max_nesting,
                        observed: depth,
                    });
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(max_depth)
}

/// Parse a JSON body under the nesting bound. A body that is not JSON is an
/// extraction failure (`ExtractionIncomplete`), never a panic and never a
/// partial value.
pub fn parse_bounded_json(
    bytes: &[u8],
    max_nesting: u32,
) -> Result<serde_json::Value, AcquisitionError> {
    check_json_nesting(bytes, max_nesting)?;
    serde_json::from_slice(bytes).map_err(|_| AcquisitionError::ExtractionIncomplete)
}

/// Store a fresh acquisition in the cache. A `304 Not Modified` stores
/// nothing — the caller keeps its existing snapshot (no duplicate snapshot,
/// no re-parse).
pub fn store_acquisition(
    cache: &mut crate::cache::CacheState,
    key: CacheKey,
    class: FieldFreshnessClass,
    acquisition: &HttpAcquisition,
) -> bool {
    if acquisition.not_modified {
        return false;
    }
    let Some(provenance) = &acquisition.provenance else {
        return false;
    };
    cache.insert(
        key,
        CacheEntry {
            observed_at_ms: provenance.observed_at_ms,
            class,
            etag: acquisition.validators.etag.clone(),
            last_modified: acquisition.validators.last_modified.clone(),
            content_digest: Some(provenance.content_digest.clone()),
            content: acquisition.json.clone(),
        },
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validation_rejects_hostile_shapes() {
        assert!(validate_fetch_url("https://h/p", 4096).is_ok());
        assert!(validate_fetch_url("http://127.0.0.1:9911/x", 4096).is_ok());
        for bad in [
            "",
            "ftp://h/p",
            "file:///etc/passwd",
            "https://",
            "https://user:pass@h/p",
            "https://h:notaport/p",
            "https://h/p\nx",
            "not a url",
        ] {
            assert!(
                validate_fetch_url(bad, 4096).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(validate_fetch_url(&format!("https://h/{}", "x".repeat(5000)), 4096).is_err());
    }

    #[test]
    fn redirect_resolution_handles_every_shape() {
        let base = "https://h/a/b/c?q=1";
        assert_eq!(
            resolve_redirect(base, "https://other/x", 4096).unwrap(),
            "https://other/x"
        );
        assert_eq!(
            resolve_redirect(base, "//other/x", 4096).unwrap(),
            "https://other/x"
        );
        assert_eq!(
            resolve_redirect(base, "/root", 4096).unwrap(),
            "https://h/root"
        );
        assert_eq!(
            resolve_redirect(base, "sibling", 4096).unwrap(),
            "https://h/a/b/sibling"
        );
        assert_eq!(
            resolve_redirect(base, "../up", 4096).unwrap(),
            "https://h/a/up"
        );
        assert_eq!(
            resolve_redirect(base, "../../../past-root", 4096).unwrap(),
            "https://h/past-root"
        );
        assert_eq!(
            resolve_redirect(base, "./same?z=2", 4096).unwrap(),
            "https://h/a/b/same?z=2"
        );
    }

    #[test]
    fn redirects_cannot_smuggle_other_schemes_or_credentials() {
        let base = "https://h/a";
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
        ] {
            assert!(
                resolve_redirect(base, bad, 4096).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(resolve_redirect(base, "//user:pass@h/x", 4096).is_err());
        assert!(resolve_redirect(base, "", 4096).is_err());
        assert!(resolve_redirect(base, &"a".repeat(9000), 4096).is_err());
    }

    #[test]
    fn nesting_scan_is_string_aware_and_bounded() {
        assert_eq!(check_json_nesting(br#"{"a":1}"#, 2).unwrap(), 1);
        assert_eq!(check_json_nesting(br#"{"a":[[[1]]]}"#, 8).unwrap(), 4);
        // Braces inside strings do not count.
        assert_eq!(check_json_nesting(br#"{"a":"}}}}"}"#, 1).unwrap(), 1);
        assert_eq!(check_json_nesting(br#"{"a":"\"}}}"}"#, 1).unwrap(), 1);
        let deep = format!("{}1{}", "[".repeat(40), "]".repeat(40));
        assert_eq!(
            check_json_nesting(deep.as_bytes(), 32).unwrap_err(),
            AcquisitionError::NestingTooDeep {
                limit: 32,
                observed: 33
            }
        );
        assert_eq!(
            parse_bounded_json(deep.as_bytes(), 32).unwrap_err(),
            AcquisitionError::NestingTooDeep {
                limit: 32,
                observed: 33
            }
        );
        assert!(parse_bounded_json(b"<<<not json>>>", 32).is_err());
    }
}
