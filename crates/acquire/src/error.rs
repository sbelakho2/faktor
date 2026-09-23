//! Typed acquisition failures and their retry classification.
//!
//! There is no generic "request failed": every failure mode of the
//! acquisition runtime is a distinct [`AcquisitionError`] variant carrying
//! the machine-readable fields a retry, throttle or planner decision needs
//! (`retry_after_ms`, `reset_ms`, `until_ms`, the verification kind, the
//! bound that fired). The variant set mirrors the acquisition-relevant
//! classes of `docs/acquire.md` §15; the domain-specific spellings for a
//! missing item or an ambiguous variant belong to the domain layer above
//! this crate, never here.
//!
//! Retry classification is total and documented: [`AcquisitionRetryClass`]
//! is what the bounded retry loop consults, and only `Transient` is ever
//! retried automatically. Authentication, verification, rate-limit and quota
//! failures surface immediately — the runtime never sleeps through them and
//! never circumvents them.

use faktor_provider::egress::EgressError;
use serde::{Deserialize, Serialize};

/// What kind of human verification a source demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    /// A challenge (CAPTCHA-like or otherwise) must be solved by a human.
    Challenge,
    /// The human must consent/approve something out of band.
    Consent,
    /// An unclassified challenge.
    Unknown,
}

/// How a failure must be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionRetryClass {
    /// Retry below the agent with bounded jitter (connect reset, 502, ...).
    Transient,
    /// Wait for the named retry window (`retry_after_ms`).
    RateLimited,
    /// Wait for the quota window to reset (`reset_ms`).
    QuotaExhausted,
    /// Surface immediately; a human must authenticate or verify.
    HumanRequired,
    /// The caller cancelled or the deadline expired; never retry.
    Aborted,
    /// Never retry automatically.
    Permanent,
}

/// A typed acquisition failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum AcquisitionError {
    /// The acquisition path is disabled by configuration.
    #[error("acquisition is disabled")]
    Disabled,
    /// The request itself is invalid (bad identity, empty fields, bad URL).
    #[error("invalid acquisition request: {detail}")]
    InvalidRequest {
        /// What was wrong.
        detail: String,
    },
    /// Credentials are required and missing/expired.
    #[error("credentials are required")]
    AuthenticationRequired,
    /// A human verification step is required.
    #[error("human verification is required ({kind:?})")]
    VerificationRequired {
        /// The kind of verification.
        kind: VerificationKind,
    },
    /// The source rate-limited the request.
    #[error("rate limited; retry after {retry_after_ms} ms")]
    RateLimited {
        /// How long to wait before retrying.
        retry_after_ms: u64,
    },
    /// The quota window is exhausted.
    #[error("quota exhausted; resets at {reset_ms}")]
    QuotaExhausted {
        /// When the quota resets.
        reset_ms: u64,
    },
    /// The path is cooling down after failures.
    #[error("cooling down until {until_ms}")]
    CoolingDown {
        /// Until when.
        until_ms: u64,
    },
    /// The egress path refused or is unavailable.
    #[error("egress is unavailable")]
    EgressUnavailable,
    /// A network timeout (also the class of a connect reset).
    #[error("network timeout")]
    NetworkTimeout,
    /// The upstream API is unavailable (5xx).
    #[error("api unavailable")]
    ApiUnavailable,
    /// The browser backend is unavailable.
    #[error("browser unavailable")]
    BrowserUnavailable,
    /// The browser backend crashed.
    #[error("browser crashed")]
    BrowserCrashed,
    /// The requested datum does not exist (HTTP 404/410).
    #[error("the requested datum was not found")]
    NotFound,
    /// Extraction could not complete (unparseable or partial response).
    #[error("extraction incomplete")]
    ExtractionIncomplete,
    /// Extractors disagreed on the same field.
    #[error("extractors disagreed")]
    ExtractionConflict,
    /// The response exceeded the configured byte bound.
    #[error("response exceeds {limit_bytes} bytes")]
    ResponseTooLarge {
        /// The bound that fired.
        limit_bytes: u64,
    },
    /// The JSON document nested deeper than the configured bound.
    #[error("json nesting {observed} exceeds {limit}")]
    NestingTooDeep {
        /// The configured bound.
        limit: u32,
        /// The observed depth.
        observed: u32,
    },
    /// The redirect chain exceeded the configured bound.
    #[error("redirect chain exceeds {limit} hops")]
    TooManyRedirects {
        /// The configured hop bound.
        limit: u32,
    },
    /// The caller cancelled.
    #[error("cancelled")]
    Cancelled,
    /// The deadline expired.
    #[error("deadline expired")]
    Deadline,
    /// The durable store failed.
    #[error("durable store failure")]
    Store,
}

impl AcquisitionError {
    /// The stable wire label.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::AuthenticationRequired => "authentication_required",
            Self::VerificationRequired { .. } => "verification_required",
            Self::RateLimited { .. } => "rate_limited",
            Self::QuotaExhausted { .. } => "quota_exhausted",
            Self::CoolingDown { .. } => "cooling_down",
            Self::EgressUnavailable => "egress_unavailable",
            Self::NetworkTimeout => "network_timeout",
            Self::ApiUnavailable => "api_unavailable",
            Self::BrowserUnavailable => "browser_unavailable",
            Self::BrowserCrashed => "browser_crashed",
            Self::NotFound => "not_found",
            Self::ExtractionIncomplete => "extraction_incomplete",
            Self::ExtractionConflict => "extraction_conflict",
            Self::ResponseTooLarge { .. } => "response_too_large",
            Self::NestingTooDeep { .. } => "nesting_too_deep",
            Self::TooManyRedirects { .. } => "too_many_redirects",
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
            Self::Store => "store",
        }
    }

    /// The retry classification of this failure.
    pub const fn retry_class(&self) -> AcquisitionRetryClass {
        match self {
            // The only automatically retried class: transport-level blips.
            Self::NetworkTimeout | Self::ApiUnavailable | Self::BrowserCrashed => {
                AcquisitionRetryClass::Transient
            }
            // Wait for the named window; the retry loop never sleeps through
            // it, the caller/planner decides.
            Self::RateLimited { .. } => AcquisitionRetryClass::RateLimited,
            Self::QuotaExhausted { .. } => AcquisitionRetryClass::QuotaExhausted,
            // A human must act; surface immediately.
            Self::AuthenticationRequired | Self::VerificationRequired { .. } => {
                AcquisitionRetryClass::HumanRequired
            }
            // Aborted work is never retried.
            Self::Cancelled | Self::Deadline => AcquisitionRetryClass::Aborted,
            Self::Disabled
            | Self::InvalidRequest { .. }
            | Self::CoolingDown { .. }
            | Self::EgressUnavailable
            | Self::BrowserUnavailable
            | Self::NotFound
            | Self::ExtractionIncomplete
            | Self::ExtractionConflict
            | Self::ResponseTooLarge { .. }
            | Self::NestingTooDeep { .. }
            | Self::TooManyRedirects { .. }
            | Self::Store => AcquisitionRetryClass::Permanent,
        }
    }

    /// Whether the bounded retry loop may retry this failure.
    pub const fn is_transient(&self) -> bool {
        matches!(self.retry_class(), AcquisitionRetryClass::Transient)
    }
}

/// Build an [`AcquisitionError::InvalidRequest`].
pub fn invalid(detail: impl Into<String>) -> AcquisitionError {
    AcquisitionError::InvalidRequest {
        detail: detail.into(),
    }
}

/// Map the checked-egress refusal into the acquisition vocabulary.
///
/// A denied destination or an unsupported scheme is permanent (retrying a
/// policy refusal can never succeed); a transport-level failure is the
/// transient network class; the materialization bound maps onto the
/// acquisition response bound.
impl From<EgressError> for AcquisitionError {
    fn from(err: EgressError) -> Self {
        match err {
            EgressError::Denied { .. } => AcquisitionError::EgressUnavailable,
            EgressError::Transport(_) => AcquisitionError::NetworkTimeout,
            EgressError::ResponseTooLarge { limit_bytes } => {
                AcquisitionError::ResponseTooLarge { limit_bytes }
            }
            other => invalid(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_is_total_and_matches_the_spec_classes() {
        let transient = [
            AcquisitionError::NetworkTimeout,
            AcquisitionError::ApiUnavailable,
            AcquisitionError::BrowserCrashed,
        ];
        for err in transient {
            assert!(err.is_transient(), "{err} must be transient");
        }
        let immediate = [
            AcquisitionError::AuthenticationRequired,
            AcquisitionError::VerificationRequired {
                kind: VerificationKind::Challenge,
            },
            AcquisitionError::RateLimited { retry_after_ms: 10 },
            AcquisitionError::QuotaExhausted { reset_ms: 10 },
            AcquisitionError::Cancelled,
            AcquisitionError::Deadline,
            AcquisitionError::Disabled,
            AcquisitionError::EgressUnavailable,
            AcquisitionError::ResponseTooLarge { limit_bytes: 1 },
        ];
        for err in immediate {
            assert!(!err.is_transient(), "{err} must not be transient");
        }
    }

    #[test]
    fn egress_refusals_map_to_typed_acquisition_errors() {
        let denied = EgressError::Denied {
            url: "http://127.0.0.1:1".into(),
            reason: faktor_security::destination::DeniedReason {
                rule_fired: None,
                matched: faktor_security::destination::RuleMatch::None,
            },
        };
        assert_eq!(
            AcquisitionError::from(denied),
            AcquisitionError::EgressUnavailable
        );
        assert_eq!(
            AcquisitionError::from(EgressError::Transport("connect reset".into())),
            AcquisitionError::NetworkTimeout
        );
        assert_eq!(
            AcquisitionError::from(EgressError::ResponseTooLarge { limit_bytes: 4096 }),
            AcquisitionError::ResponseTooLarge { limit_bytes: 4096 }
        );
        assert!(matches!(
            AcquisitionError::from(EgressError::UnparseableUrl(
                faktor_provider::egress::UrlRejectReason::ParseError
            )),
            AcquisitionError::InvalidRequest { .. }
        ));
    }

    #[test]
    fn wire_labels_are_stable() {
        assert_eq!(AcquisitionError::Deadline.as_str(), "deadline");
        assert_eq!(
            AcquisitionError::NestingTooDeep {
                limit: 2,
                observed: 3
            }
            .as_str(),
            "nesting_too_deep"
        );
    }
}
