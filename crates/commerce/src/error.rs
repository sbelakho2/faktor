//! Typed source errors and connector health.
//!
//! There is no generic "scrape failed": every failure mode of the
//! acquisition contract is a distinct [`SourceError`] variant, and every
//! variant carries the machine-readable fields a retry/throttle policy
//! needs (`retry_after_ms`, `reset_ms`, `until_ms`, the verification kind).
//! [`ConnectorHealth`] is the planner-facing view of the same conditions.

use serde::{Deserialize, Serialize};

use crate::text::Text;

/// What kind of human verification a source demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    /// A CAPTCHA.
    Captcha,
    /// A login.
    Login,
    /// A second factor.
    TwoFactor,
    /// An emailed code.
    EmailCode,
    /// A text-message code.
    SmsCode,
    /// A manual/operator step.
    Manual,
    /// An unclassified challenge.
    Unknown,
}

/// How a failure should be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRetryClass {
    /// Retry below the agent with bounded jitter.
    Transient,
    /// Wait for the named retry window.
    RateLimited,
    /// Wait for the quota window to reset.
    QuotaExhausted,
    /// Surface immediately; a human must authenticate or verify.
    HumanRequired,
    /// The caller cancelled or the deadline expired; do not retry.
    Aborted,
    /// Never retry automatically.
    Permanent,
}

/// A typed acquisition failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum SourceError {
    /// The source is disabled by configuration.
    Disabled,
    /// The request itself is invalid.
    InvalidRequest,
    /// Credentials are required.
    AuthenticationRequired,
    /// A human verification is required.
    VerificationRequired {
        /// The kind of verification.
        kind: VerificationKind,
    },
    /// The source rate-limited the request.
    RateLimited {
        /// How long to wait before retrying.
        retry_after_ms: u64,
    },
    /// The quota is exhausted.
    QuotaExhausted {
        /// When the quota resets.
        reset_ms: u64,
    },
    /// The source is in a cooling-down period after failures.
    CoolingDown {
        /// Until when.
        until_ms: u64,
    },
    /// The egress path is unavailable.
    EgressUnavailable,
    /// A network timeout.
    NetworkTimeout,
    /// The API is unavailable.
    ApiUnavailable,
    /// The browser is unavailable.
    BrowserUnavailable,
    /// The browser crashed.
    BrowserCrashed,
    /// The product does not exist.
    ProductNotFound,
    /// The variant mapping is ambiguous.
    VariantAmbiguous,
    /// Extraction was incomplete.
    ExtractionIncomplete,
    /// Extractors disagreed.
    ExtractionConflict,
    /// The response exceeded the bounds.
    ResponseTooLarge,
    /// The caller cancelled.
    Cancelled,
    /// The deadline expired.
    Deadline,
    /// The durable store failed.
    Store,
}

impl SourceError {
    /// The stable wire label.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::InvalidRequest => "invalid_request",
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
            Self::ProductNotFound => "product_not_found",
            Self::VariantAmbiguous => "variant_ambiguous",
            Self::ExtractionIncomplete => "extraction_incomplete",
            Self::ExtractionConflict => "extraction_conflict",
            Self::ResponseTooLarge => "response_too_large",
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
            Self::Store => "store",
        }
    }

    /// The retry classification.
    pub const fn retry_class(&self) -> SourceRetryClass {
        match self {
            Self::NetworkTimeout
            | Self::ApiUnavailable
            | Self::BrowserCrashed
            | Self::EgressUnavailable
            | Self::BrowserUnavailable => SourceRetryClass::Transient,
            Self::RateLimited { .. } | Self::CoolingDown { .. } => SourceRetryClass::RateLimited,
            Self::QuotaExhausted { .. } => SourceRetryClass::QuotaExhausted,
            Self::AuthenticationRequired | Self::VerificationRequired { .. } => {
                SourceRetryClass::HumanRequired
            }
            Self::Cancelled | Self::Deadline => SourceRetryClass::Aborted,
            Self::Disabled
            | Self::InvalidRequest
            | Self::ProductNotFound
            | Self::VariantAmbiguous
            | Self::ExtractionIncomplete
            | Self::ExtractionConflict
            | Self::ResponseTooLarge
            | Self::Store => SourceRetryClass::Permanent,
        }
    }

    /// True when the runtime may retry below the agent.
    pub const fn is_transient(&self) -> bool {
        matches!(self.retry_class(), SourceRetryClass::Transient)
    }

    /// True when the failure must surface to the agent immediately
    /// (authentication, CAPTCHA/verification, quota).
    pub const fn surfaces_immediately(&self) -> bool {
        matches!(
            self.retry_class(),
            SourceRetryClass::HumanRequired | SourceRetryClass::QuotaExhausted
        )
    }

    /// The retry window named by the error, when it has one.
    pub const fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after_ms } => Some(*retry_after_ms),
            Self::CoolingDown { until_ms } => Some(*until_ms),
            Self::QuotaExhausted { reset_ms } => Some(*reset_ms),
            _ => None,
        }
    }

    /// The planner-facing health of this failure.
    pub fn into_health(self) -> ConnectorHealth {
        ConnectorHealth::from_error(&self)
    }
}

/// A [`SourceError`] renders as its stable label (never a free-form message),
/// so logs and metrics key on the typed variant.
impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())?;
        match self {
            Self::RateLimited { retry_after_ms } => write!(f, " (retry after {retry_after_ms} ms)"),
            Self::QuotaExhausted { reset_ms } => write!(f, " (resets at {reset_ms} ms)"),
            Self::CoolingDown { until_ms } => write!(f, " (until {until_ms} ms)"),
            Self::VerificationRequired { kind } => write!(f, " ({})", verification_label(*kind)),
            _ => Ok(()),
        }
    }
}

impl std::error::Error for SourceError {}

/// The planner-facing health of a connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectorHealth {
    /// Fully available.
    Healthy,
    /// Available but degraded.
    Degraded {
        /// Why.
        reason: Text<256>,
    },
    /// In a cooling-down period.
    CoolingDown {
        /// Until when.
        until_ms: u64,
    },
    /// Rate-limited until the named time.
    RateLimited {
        /// Until when.
        until_ms: u64,
    },
    /// Credentials are required.
    AuthenticationRequired,
    /// A human verification is pending.
    VerificationRequired {
        /// The challenge label.
        challenge: Text<256>,
    },
    /// The quota is exhausted.
    QuotaExhausted {
        /// When the quota resets.
        reset_ms: u64,
    },
    /// Not available at all.
    Unavailable,
}

impl ConnectorHealth {
    /// True when the connector may be planned against right now.
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded { .. })
    }

    /// The time the condition is expected to clear, when known.
    pub const fn until_ms(&self) -> Option<u64> {
        match self {
            Self::CoolingDown { until_ms } | Self::RateLimited { until_ms } => Some(*until_ms),
            Self::QuotaExhausted { reset_ms } => Some(*reset_ms),
            _ => None,
        }
    }

    /// Map a failure to the health the planner should see.
    pub fn from_error(error: &SourceError) -> Self {
        match error {
            SourceError::Disabled | SourceError::EgressUnavailable => Self::Unavailable,
            SourceError::InvalidRequest => Self::Degraded {
                reason: Text::from_static_label("invalid request"),
            },
            SourceError::AuthenticationRequired => Self::AuthenticationRequired,
            SourceError::VerificationRequired { kind } => Self::VerificationRequired {
                challenge: Text::from_static_label(verification_label(*kind)),
            },
            SourceError::RateLimited { retry_after_ms } => Self::RateLimited {
                until_ms: *retry_after_ms,
            },
            SourceError::QuotaExhausted { reset_ms } => Self::QuotaExhausted {
                reset_ms: *reset_ms,
            },
            SourceError::CoolingDown { until_ms } => Self::CoolingDown {
                until_ms: *until_ms,
            },
            SourceError::NetworkTimeout
            | SourceError::ApiUnavailable
            | SourceError::BrowserUnavailable
            | SourceError::BrowserCrashed
            | SourceError::ExtractionIncomplete
            | SourceError::ExtractionConflict
            | SourceError::ResponseTooLarge
            | SourceError::Store
            | SourceError::ProductNotFound
            | SourceError::VariantAmbiguous
            | SourceError::Cancelled
            | SourceError::Deadline => Self::Degraded {
                reason: Text::from_static_label(error.as_str()),
            },
        }
    }

    /// The failure this health corresponds to, when it is not available.
    pub fn to_error(&self) -> Option<SourceError> {
        match self {
            Self::Healthy | Self::Degraded { .. } => None,
            Self::CoolingDown { until_ms } => Some(SourceError::CoolingDown {
                until_ms: *until_ms,
            }),
            Self::RateLimited { until_ms } => Some(SourceError::RateLimited {
                retry_after_ms: *until_ms,
            }),
            Self::AuthenticationRequired => Some(SourceError::AuthenticationRequired),
            Self::VerificationRequired { .. } => Some(SourceError::VerificationRequired {
                kind: VerificationKind::Unknown,
            }),
            Self::QuotaExhausted { reset_ms } => Some(SourceError::QuotaExhausted {
                reset_ms: *reset_ms,
            }),
            Self::Unavailable => Some(SourceError::EgressUnavailable),
        }
    }
}

fn verification_label(kind: VerificationKind) -> &'static str {
    match kind {
        VerificationKind::Captcha => "captcha",
        VerificationKind::Login => "login",
        VerificationKind::TwoFactor => "two_factor",
        VerificationKind::EmailCode => "email_code",
        VerificationKind::SmsCode => "sms_code",
        VerificationKind::Manual => "manual",
        VerificationKind::Unknown => "unknown",
    }
}

/// Every [`SourceError`] label.
pub const SOURCE_ERROR_LABELS: &[&str] = &[
    "disabled",
    "invalid_request",
    "authentication_required",
    "verification_required",
    "rate_limited",
    "quota_exhausted",
    "cooling_down",
    "egress_unavailable",
    "network_timeout",
    "api_unavailable",
    "browser_unavailable",
    "browser_crashed",
    "product_not_found",
    "variant_ambiguous",
    "extraction_incomplete",
    "extraction_conflict",
    "response_too_large",
    "cancelled",
    "deadline",
    "store",
];

/// Every [`VerificationKind`] label.
pub const VERIFICATION_KIND_LABELS: &[&str] = &[
    "captcha",
    "login",
    "two_factor",
    "email_code",
    "sms_code",
    "manual",
    "unknown",
];
