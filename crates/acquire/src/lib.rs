//! faktor-acquire — the generic acquisition runtime of Faktor Acquire.
//!
//! This crate is deliberately domain-free: it knows acquisition mechanisms,
//! freshness, quotas, health, retries, coalescing, provenance and bounded
//! HTTP, and nothing about any particular kind of datum. It hard-codes no
//! site name, contains no domain vocabulary, and the layer above supplies the
//! semantics.
//!
//! Invariants, each enforced by a test in this crate:
//!
//! * **No model dependency.** The dependency graph is `faktor-core` +
//!   `faktor-provider` plus small utility crates; no agent runtime, no model
//!   router and no model adapter is reachable (`tests/scan.rs` walks
//!   `Cargo.lock` transitively).
//! * **One transport.** Every outbound request goes through the injected
//!   `Arc<dyn HttpTransport>` (the checked egress authority). This crate
//!   constructs no client and executes no raw request itself.
//! * **Bounded everything.** Response bytes, JSON nesting, redirect hops,
//!   retry attempts, backoff, quota windows, identity lengths and cache ages
//!   are all bounded, and every bound has a typed error.
//! * **Deterministic planning.** Identical inputs yield identical plans; the
//!   mechanism order is the documented precedence order and no site-specific
//!   knowledge is consulted.
//! * **No silent circumvention.** Substituting a non-API mechanism for a
//!   quota-exhausted official API requires the explicit opt-in
//!   [`SubstitutionPolicy`]; the default refuses with the typed
//!   [`AcquisitionError::QuotaExhausted`].
//!
//! Module map:
//!
//! | module | contents |
//! |---|---|
//! | [`request`] | [`AcquisitionRequest`], [`RequestedFields`], [`RequestedFreshness`], identity normalization |
//! | [`mechanism`] | [`AcquisitionMechanism`] and the documented precedence |
//! | [`capability`] | [`ConnectorCapabilities`], [`CredentialAvailability`] |
//! | [`health`] | [`ConnectorHealth`], [`HealthTracker`], [`ExtractionHealth`] |
//! | [`quota`] | [`QuotaState`], windows, `Retry-After` |
//! | [`cache`] | field-level freshness classes, conditional validators |
//! | [`planner`] | [`AcquisitionPlanner`] → [`AcquisitionPlan`] |
//! | [`http`] | bounded direct HTTP through the injected transport |
//! | [`retry`] | bounded, transient-only retries with jitter |
//! | [`coalesce`] | one external request, N awaiters |
//! | [`ctx`] | [`AcquireCtx`]: deadline, cancellation, quota, health, transport |
//! | [`provenance`] | origin, observation time, content digest |
//! | [`error`] | typed [`AcquisitionError`] and its retry classes |

pub mod cache;
pub mod capability;
pub mod coalesce;
pub mod ctx;
pub mod error;
pub mod health;
pub mod http;
pub mod mechanism;
pub mod planner;
pub mod provenance;
pub mod quota;
pub mod request;
pub mod retry;

pub use cache::{
    freshness_class, CacheCoverage, CacheEntry, CacheKey, CacheState, ConditionalValidators,
    FieldFreshnessClass, FreshnessClassTtl,
};
pub use capability::{
    CapabilityLevel, ConnectorCapabilities, CredentialAvailability, CredentialStatus,
};
pub use coalesce::{Coalescer, CoalescerStats};
pub use ctx::{AcquireCtx, DEFAULT_QUOTA_LIMIT};
pub use error::{invalid, AcquisitionError, AcquisitionRetryClass, VerificationKind};
pub use health::{
    ConnectorHealth, ExtractionHealth, ExtractionOutcome, HealthPolicy, HealthTracker,
    StrategyRecord,
};
pub use http::{
    check_json_nesting, fetch_direct_http, parse_bounded_json, resolve_redirect, store_acquisition,
    validate_fetch_url, DirectHttpPolicy, HttpAcquisition, HttpFetch,
};
pub use mechanism::{AcquisitionMechanism, MECHANISM_PRECEDENCE};
pub use planner::{
    AcquisitionPlan, AcquisitionPlanner, PlanDecision, PlanNote, PlannerInputs, SubstitutionPolicy,
};
pub use provenance::{content_digest, AcquisitionProvenance};
pub use quota::{parse_retry_after_seconds, QuotaState, QuotaWindow, MAX_THROTTLE_SCALE};
pub use request::{
    normalize_identity, AcquisitionKey, AcquisitionRequest, RequestedField, RequestedFields,
    RequestedFreshness, ALL_REQUESTED_FIELDS, MAX_ACCOUNT_SCOPE_BYTES, MAX_IDENTITY_BYTES,
};
pub use retry::{run_with_retry, AcquisitionRetryPolicy};

#[cfg(test)]
mod tests;
