//! The browser-fallback seam: policy-gated, never quota evasion.
//!
//! Spec §10: "Browser fallback is for legitimate field availability/
//! resilience, never quota evasion; API and browser fallback are
//! independently circuit-broken. API-first: do not scrape around official
//! interfaces."
//!
//! This module makes that rule structural instead of a review comment:
//!
//! * [`FallbackCause`] enumerates the only legitimate triggers — a field the
//!   API does not expose, an API outage, or data that is only visible to an
//!   authenticated account. A rate limit or an exhausted quota is *also*
//!   representable so the decision function can refuse it explicitly.
//! * [`fallback_decision`] is a pure function: `RateLimited` and
//!   `QuotaExhausted` are refused as [`FallbackRefusal::QuotaEvasion`] no
//!   matter how the policy is configured. There is no code path from a quota
//!   error to the browser.
//! * The policy flag defaults to **off** ([`FallbackPolicy::default`]), and
//!   the provider is injected (`Arc<dyn BrowserFallback>`) — the connectors
//!   never launch Chromium, and `faktor-browser` (spec §9) stays the only
//!   authority that does.
//!
//! `faktor-browser`'s egress broker applies the per-connector destination
//! policy to whatever the provider does; this crate never widens it.

use async_trait::async_trait;
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{SourceError, SourceId};

use crate::context::{AcquireCtx, ConnectorEventKind};

/// Maximum fields in one fallback observation.
pub const MAX_FALLBACK_FIELDS: usize = 16;

/// The browser-fallback policy flag (default off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FallbackPolicy {
    enabled: bool,
}

impl FallbackPolicy {
    /// The default: browser fallback disabled.
    pub const fn disabled() -> Self {
        Self { enabled: false }
    }

    /// Browser fallback enabled for the legitimate causes only.
    pub const fn enabled() -> Self {
        Self { enabled: true }
    }

    /// Whether the flag is on.
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }
}

/// Why a connector is considering the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackCause {
    /// A field the API does not expose (legitimate availability).
    FieldAbsentFromApi,
    /// The API is down or unreachable (legitimate resilience).
    ApiUnavailable,
    /// Data that is only visible to an authenticated account.
    AccountVisibleData,
    /// The source rate-limited the API. **Never** a fallback cause.
    RateLimited {
        /// The stated retry window.
        retry_after_ms: u64,
    },
    /// The quota is exhausted. **Never** a fallback cause.
    QuotaExhausted {
        /// When the quota resets.
        reset_ms: u64,
    },
}

impl FallbackCause {
    /// True for the causes that would be quota evasion.
    pub const fn is_quota_evasion(self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::QuotaExhausted { .. })
    }
}

/// Why the browser was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackRefusal {
    /// The policy flag is off (the default).
    PolicyDisabled,
    /// The trigger was a rate limit or an exhausted quota.
    QuotaEvasion,
    /// No browser provider was injected.
    NoBrowserProvider,
}

/// The outcome of the pure fallback decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackDecision {
    /// Use the browser for this cause.
    UseBrowser,
    /// Do not use the browser, for this reason.
    Refused(FallbackRefusal),
}

/// The pure decision function. Quota causes are refused unconditionally.
pub fn fallback_decision(
    policy: FallbackPolicy,
    provider_present: bool,
    cause: FallbackCause,
) -> FallbackDecision {
    if cause.is_quota_evasion() {
        return FallbackDecision::Refused(FallbackRefusal::QuotaEvasion);
    }
    if !policy.is_enabled() {
        return FallbackDecision::Refused(FallbackRefusal::PolicyDisabled);
    }
    if !provider_present {
        return FallbackDecision::Refused(FallbackRefusal::NoBrowserProvider);
    }
    FallbackDecision::UseBrowser
}

/// One browser observation: bounded named fields, all untrusted data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackObservation {
    values: Vec<(Text<64>, Text<512>)>,
}

impl FallbackObservation {
    /// Validate and bound a field list.
    pub fn new(values: Vec<(Text<64>, Text<512>)>) -> Result<Self, SourceError> {
        if values.len() > MAX_FALLBACK_FIELDS {
            return Err(SourceError::ExtractionIncomplete);
        }
        Ok(Self { values })
    }

    /// One field value.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(field, _)| field.as_str() == name)
            .map(|(_, value)| value.as_str())
    }

    /// The field count.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True when no fields were observed.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// The injected browser authority seam (implemented by the acquisition
/// runtime over `faktor-browser`; never by a connector).
#[async_trait]
pub trait BrowserFallback: Send + Sync {
    /// Observe the wanted fields on one page. The provider is responsible
    /// for the destination policy, bounded capture, verification handling
    /// and returning typed errors.
    async fn observe(
        &self,
        ctx: &AcquireCtx,
        source: &SourceId,
        url: &CanonicalUrl,
        wanted: &[&'static str],
    ) -> Result<FallbackObservation, SourceError>;
}

/// Attempt the browser fallback for `cause`.
///
/// Returns `Ok(None)` when the decision refuses the fallback (the caller
/// keeps its API-derived result or its original typed error). Returns the
/// provider's typed error when the browser was allowed but failed. Records
/// the decision in diagnostics, so a refusal is observable.
pub(crate) async fn attempt(
    ctx: &AcquireCtx,
    source: &SourceId,
    cause: FallbackCause,
    url: &CanonicalUrl,
    wanted: &[&'static str],
) -> Result<Option<FallbackObservation>, SourceError> {
    let provider_present = ctx.browser().is_some();
    match fallback_decision(ctx.fallback_policy(), provider_present, cause) {
        FallbackDecision::Refused(refusal) => {
            ctx.record(
                source,
                "browser_fallback",
                ConnectorEventKind::Fallback,
                None,
                Some(refusal_label(refusal)),
            );
            Ok(None)
        }
        FallbackDecision::UseBrowser => {
            let Some(provider) = ctx.browser() else {
                return Ok(None);
            };
            ctx.record(
                source,
                "browser_fallback",
                ConnectorEventKind::Fallback,
                None,
                Some(cause_label(cause)),
            );
            let observation = provider.observe(ctx, source, url, wanted).await?;
            Ok(Some(observation))
        }
    }
}

/// The stable label of a refusal.
pub const fn refusal_label(refusal: FallbackRefusal) -> &'static str {
    match refusal {
        FallbackRefusal::PolicyDisabled => "policy_disabled",
        FallbackRefusal::QuotaEvasion => "quota_evasion_refused",
        FallbackRefusal::NoBrowserProvider => "no_browser_provider",
    }
}

/// The stable label of a cause.
pub const fn cause_label(cause: FallbackCause) -> &'static str {
    match cause {
        FallbackCause::FieldAbsentFromApi => "field_absent_from_api",
        FallbackCause::ApiUnavailable => "api_unavailable",
        FallbackCause::AccountVisibleData => "account_visible_data",
        FallbackCause::RateLimited { .. } => "rate_limited",
        FallbackCause::QuotaExhausted { .. } => "quota_exhausted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_causes_are_refused_even_with_policy_on_and_provider() {
        for cause in [
            FallbackCause::RateLimited {
                retry_after_ms: 1_000,
            },
            FallbackCause::QuotaExhausted { reset_ms: 9_999 },
        ] {
            assert!(cause.is_quota_evasion());
            for policy in [FallbackPolicy::disabled(), FallbackPolicy::enabled()] {
                for provider in [false, true] {
                    assert_eq!(
                        fallback_decision(policy, provider, cause),
                        FallbackDecision::Refused(FallbackRefusal::QuotaEvasion),
                        "{cause:?} must never reach the browser"
                    );
                }
            }
        }
    }

    #[test]
    fn legitimate_causes_follow_the_policy_flag_and_provider() {
        let causes = [
            FallbackCause::FieldAbsentFromApi,
            FallbackCause::ApiUnavailable,
            FallbackCause::AccountVisibleData,
        ];
        for cause in causes {
            assert!(!cause.is_quota_evasion());
            assert_eq!(
                fallback_decision(FallbackPolicy::default(), true, cause),
                FallbackDecision::Refused(FallbackRefusal::PolicyDisabled),
                "the default is off"
            );
            assert_eq!(
                fallback_decision(FallbackPolicy::enabled(), false, cause),
                FallbackDecision::Refused(FallbackRefusal::NoBrowserProvider)
            );
            assert_eq!(
                fallback_decision(FallbackPolicy::enabled(), true, cause),
                FallbackDecision::UseBrowser
            );
        }
    }

    #[test]
    fn observations_are_bounded() {
        let mut values = Vec::new();
        for index in 0..MAX_FALLBACK_FIELDS {
            values.push((
                Text::<64>::new(&format!("field{index}")).unwrap(),
                Text::<512>::new("value").unwrap(),
            ));
        }
        assert!(FallbackObservation::new(values.clone()).is_ok());
        values.push((
            Text::<64>::new("overflow").unwrap(),
            Text::<512>::new("value").unwrap(),
        ));
        assert!(FallbackObservation::new(values).is_err());
    }
}
