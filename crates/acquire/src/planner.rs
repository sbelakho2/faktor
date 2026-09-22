//! The deterministic acquisition planner.
//!
//! [`AcquisitionPlanner::plan`] consumes only: the request, the connector's
//! advertised capabilities, the path's health, credential availability, the
//! quota state, the cache state, the freshness classes and the per-field
//! extraction health. It hard-codes no site, no host and no datum name.
//!
//! Decision order (`docs/acquire.md` §1):
//!
//! 1. a fresh cache entry serves the request when every requested field is
//!    fresh and the caller did not demand `Live`;
//! 2. otherwise mechanisms are walked in precedence order
//!    (`OfficialApi`, `DirectHttp`, `BrowserNetwork`, `EmbeddedState`,
//!    `Dom`) and the first mechanism that (a) covers every requested field,
//!    (b) has usable credentials, (c) is usable at `now_ms` per health, and
//!    (d) is not proven bad for every requested field by extraction health,
//!    wins;
//! 3. if the official API is blocked by quota, the plan refuses with the
//!    typed `QuotaExhausted` **unless** the explicit substitution policy
//!    allows a non-API mechanism — the default is `false`, so the runtime
//!    never silently circumvents official API limits;
//! 4. when nothing is available the plan refuses with the typed reason
//!    (or demands human verification), never with a generic failure.

use serde::{Deserialize, Serialize};

use crate::cache::{CacheCoverage, CacheState, ConditionalValidators, FreshnessClassTtl};
use crate::capability::{CapabilityLevel, ConnectorCapabilities, CredentialAvailability};
use crate::error::{invalid, AcquisitionError, VerificationKind};
use crate::health::{ConnectorHealth, ExtractionHealth};
use crate::mechanism::{AcquisitionMechanism, MECHANISM_PRECEDENCE};
use crate::quota::QuotaState;
use crate::request::{normalize_identity, AcquisitionRequest, RequestedField, RequestedFreshness};

/// Whether a non-API mechanism may substitute when the official API quota is
/// exhausted. Default: `false` (never silently circumvent API limits).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubstitutionPolicy {
    /// The explicit opt-in flag.
    pub allow_non_api_when_api_quota_exhausted: bool,
}

/// A machine-readable note attached to a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "note", rename_all = "snake_case")]
pub enum PlanNote {
    /// The chosen path is degraded but usable.
    HealthDegraded {
        /// The degradation reason.
        reason: String,
    },
    /// The API quota blocked the strongest mechanism.
    ApiQuotaExhausted {
        /// When the quota resets.
        reset_ms: u64,
    },
    /// The API quota blocked the API and substitution was refused.
    ApiQuotaExhaustedNoSubstitution,
    /// A non-API mechanism was used because substitution is enabled.
    SubstitutedMechanism {
        /// The mechanism that was skipped.
        from: AcquisitionMechanism,
        /// The mechanism that was chosen instead.
        to: AcquisitionMechanism,
    },
    /// The mechanism was skipped because its credentials are unusable.
    CredentialsUnusable {
        /// The skipped mechanism.
        mechanism: AcquisitionMechanism,
    },
    /// The mechanism does not advertise the requested fields.
    FieldsUnsupported {
        /// The skipped mechanism.
        mechanism: AcquisitionMechanism,
        /// The first field it cannot serve.
        field: RequestedField,
    },
    /// The mechanism is not usable per health at this instant.
    HealthBlocked {
        /// The skipped mechanism.
        mechanism: AcquisitionMechanism,
        /// The health label.
        health: String,
    },
    /// The mechanism is proven bad for every requested field by extraction
    /// health.
    ProvenBadStrategy {
        /// The skipped mechanism.
        mechanism: AcquisitionMechanism,
    },
    /// The chosen mechanism covers the fields only partially.
    PartialCoverage {
        /// The chosen mechanism.
        mechanism: AcquisitionMechanism,
    },
    /// The remembered best strategy for at least one field is the chosen
    /// mechanism.
    RememberedBestStrategy {
        /// The field with a remembered success.
        field: RequestedField,
    },
    /// The request was sent conditionally because a stored copy exists.
    ConditionalRevalidation,
    /// The plan demands human verification.
    VerificationRequired {
        /// The kind of verification.
        kind: VerificationKind,
    },
    /// The plan serves a stale cache entry (never presented as current).
    StaleCacheServed,
}

/// What the planner decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PlanDecision {
    /// Serve from the cache.
    ServeFromCache {
        /// Whether the served entry is stale (age beyond its TTL).
        stale: bool,
    },
    /// Acquire through one mechanism.
    Acquire {
        /// The chosen mechanism.
        mechanism: AcquisitionMechanism,
        /// Conditional validators to send, when a stored copy exists.
        conditional: Option<ConditionalValidators>,
        /// Whether this is a non-API substitute for a quota-blocked API.
        substituted: bool,
        /// Whether the chosen path is degraded.
        degraded: bool,
        /// How well the mechanism covers the requested fields.
        coverage: CapabilityLevel,
    },
    /// Stop and ask a human to verify.
    RequireVerification {
        /// The kind of verification.
        kind: VerificationKind,
    },
    /// Refuse with a typed reason.
    Refuse {
        /// The typed reason.
        error: AcquisitionError,
    },
}

/// One full plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquisitionPlan {
    /// The decision.
    pub decision: PlanDecision,
    /// The cache coverage the decision was based on.
    pub cache: CacheCoverage,
    /// The requested fields in canonical order.
    pub fields: Vec<RequestedField>,
    /// Machine-readable notes.
    pub notes: Vec<PlanNote>,
}

/// The planner's inputs, bundled so the call stays readable.
pub struct PlannerInputs<'a> {
    /// The request.
    pub request: &'a AcquisitionRequest,
    /// What the connector advertises.
    pub capabilities: &'a ConnectorCapabilities,
    /// The path health at `now_ms`.
    pub health: &'a ConnectorHealth,
    /// Credential availability.
    pub credentials: &'a CredentialAvailability,
    /// The quota state.
    pub quota: &'a QuotaState,
    /// The cache state.
    pub cache: &'a CacheState,
    /// Freshness class TTLs.
    pub ttl: &'a FreshnessClassTtl,
    /// Per-field extraction memory.
    pub field_health: &'a ExtractionHealth,
    /// The current instant.
    pub now_ms: u64,
}

/// The planner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcquisitionPlanner {
    /// The substitution policy.
    pub substitution: SubstitutionPolicy,
}

impl AcquisitionPlanner {
    /// A planner with the given substitution policy.
    pub fn new(substitution: SubstitutionPolicy) -> Self {
        Self { substitution }
    }

    /// Produce a plan. Deterministic: identical inputs yield identical plans.
    pub fn plan(&self, inputs: &PlannerInputs<'_>) -> AcquisitionPlan {
        let fields: Vec<RequestedField> = inputs.request.fields.iter().collect();
        let mut notes = Vec::new();
        if fields.is_empty() {
            return refuse(
                invalid("requested fields are empty"),
                CacheCoverage::default(),
                fields,
                notes,
            );
        }
        let identity = match normalize_identity(&inputs.request.identity) {
            Ok(identity) => identity,
            Err(error) => return refuse(error, CacheCoverage::default(), fields, notes),
        };
        let cache = inputs.cache.coverage(
            &identity,
            inputs.request.account_scope.as_deref(),
            fields.iter().copied(),
            inputs.ttl,
            inputs.now_ms,
        );

        // Step 1: fresh cache (or cache-only refusal).
        match inputs.request.freshness {
            RequestedFreshness::PreferCache if cache.all_fresh() => {
                return plan(
                    PlanDecision::ServeFromCache { stale: false },
                    cache,
                    fields,
                    notes,
                );
            }
            RequestedFreshness::CacheOnly => {
                if cache.all_present() {
                    let stale = !cache.all_fresh();
                    if stale {
                        notes.push(PlanNote::StaleCacheServed);
                    }
                    return plan(PlanDecision::ServeFromCache { stale }, cache, fields, notes);
                }
                return refuse(AcquisitionError::ExtractionIncomplete, cache, fields, notes);
            }
            _ => {}
        }

        // Step 2: mechanisms, in documented precedence order.
        let api_quota_reset = self.api_quota_reset(inputs);
        let mut substituted = false;
        let mut chosen: Option<(AcquisitionMechanism, CapabilityLevel)> = None;
        for mechanism in MECHANISM_PRECEDENCE {
            if mechanism.is_official_api() {
                if let Some(reset_ms) = api_quota_reset {
                    notes.push(PlanNote::ApiQuotaExhausted { reset_ms });
                    if !self.substitution.allow_non_api_when_api_quota_exhausted {
                        notes.push(PlanNote::ApiQuotaExhaustedNoSubstitution);
                        return refuse(
                            AcquisitionError::QuotaExhausted { reset_ms },
                            cache,
                            fields,
                            notes,
                        );
                    }
                    substituted = true;
                    continue;
                }
            }
            if !inputs
                .capabilities
                .eligible(mechanism, &inputs.request.fields)
            {
                if let Some(field) = inputs
                    .capabilities
                    .first_unsupported(mechanism, &inputs.request.fields)
                {
                    notes.push(PlanNote::FieldsUnsupported { mechanism, field });
                }
                continue;
            }
            if !inputs.credentials.usable(mechanism) {
                notes.push(PlanNote::CredentialsUnusable { mechanism });
                continue;
            }
            if !inputs.health.usable_at(inputs.now_ms) {
                notes.push(PlanNote::HealthBlocked {
                    mechanism,
                    health: inputs.health.as_str().to_string(),
                });
                continue;
            }
            if inputs.field_health.proven_bad_for_all(mechanism, &fields) {
                notes.push(PlanNote::ProvenBadStrategy { mechanism });
                continue;
            }
            let coverage = inputs
                .capabilities
                .coverage(mechanism, &inputs.request.fields);
            chosen = Some((mechanism, coverage));
            break;
        }

        let Some((mechanism, coverage)) = chosen else {
            return self.no_candidate_plan(inputs, cache, fields, notes);
        };

        // Conditional revalidation only when we hold every requested field:
        // a 304 for a request that also needs a missing field would leave a
        // hole in the result.
        let conditional = if cache.all_present() {
            let validators = inputs.cache.conditional_validators(
                &identity,
                inputs.request.account_scope.as_deref(),
                fields.iter().copied(),
            );
            if validators.is_some() {
                notes.push(PlanNote::ConditionalRevalidation);
            }
            validators
        } else {
            None
        };

        if substituted {
            notes.push(PlanNote::SubstitutedMechanism {
                from: AcquisitionMechanism::OfficialApi,
                to: mechanism,
            });
        }
        if coverage == CapabilityLevel::Partial {
            notes.push(PlanNote::PartialCoverage { mechanism });
        }
        let degraded = inputs.health.is_degraded();
        if degraded {
            notes.push(PlanNote::HealthDegraded {
                reason: inputs.health.as_str().to_string(),
            });
        }
        if let Some(field) = fields
            .iter()
            .copied()
            .find(|field| inputs.field_health.best_strategy(*field) == Some(mechanism))
        {
            notes.push(PlanNote::RememberedBestStrategy { field });
        }
        plan(
            PlanDecision::Acquire {
                mechanism,
                conditional,
                substituted,
                degraded,
                coverage,
            },
            cache,
            fields,
            notes,
        )
    }

    /// The API quota reset when the API is blocked by quota, or `None`.
    ///
    /// Only an API path the connector advertises and can authenticate counts:
    /// a connector with no official API cannot "circumvent" one.
    fn api_quota_reset(&self, inputs: &PlannerInputs<'_>) -> Option<u64> {
        let api = AcquisitionMechanism::OfficialApi;
        if !inputs.capabilities.eligible(api, &inputs.request.fields)
            || !inputs.credentials.usable(api)
        {
            return None;
        }
        if let Some(reset_ms) = inputs.health.quota_reset_ms() {
            if inputs.now_ms < reset_ms {
                return Some(reset_ms);
            }
        }
        if inputs.quota.is_exhausted_at(inputs.now_ms) {
            return Some(inputs.quota.reset_at_ms());
        }
        None
    }

    fn no_candidate_plan(
        &self,
        inputs: &PlannerInputs<'_>,
        cache: CacheCoverage,
        fields: Vec<RequestedField>,
        mut notes: Vec<PlanNote>,
    ) -> AcquisitionPlan {
        match inputs.health {
            ConnectorHealth::VerificationRequired { kind } => {
                notes.push(PlanNote::VerificationRequired { kind: *kind });
                return plan(
                    PlanDecision::RequireVerification { kind: *kind },
                    cache,
                    fields,
                    notes,
                );
            }
            ConnectorHealth::AuthenticationRequired => {
                return refuse(
                    AcquisitionError::AuthenticationRequired,
                    cache,
                    fields,
                    notes,
                );
            }
            ConnectorHealth::QuotaExhausted { reset_ms } if inputs.now_ms < *reset_ms => {
                return refuse(
                    AcquisitionError::QuotaExhausted {
                        reset_ms: *reset_ms,
                    },
                    cache,
                    fields,
                    notes,
                );
            }
            ConnectorHealth::CoolingDown { until_ms } if inputs.now_ms < *until_ms => {
                return refuse(
                    AcquisitionError::CoolingDown {
                        until_ms: *until_ms,
                    },
                    cache,
                    fields,
                    notes,
                );
            }
            ConnectorHealth::RateLimited { until_ms } if inputs.now_ms < *until_ms => {
                return refuse(
                    AcquisitionError::RateLimited {
                        retry_after_ms: until_ms - inputs.now_ms,
                    },
                    cache,
                    fields,
                    notes,
                );
            }
            ConnectorHealth::Unavailable => {
                let error = strongest_mechanism_error(inputs.capabilities);
                return refuse(error, cache, fields, notes);
            }
            _ => {}
        }
        if let Some(reset_ms) = self.api_quota_reset(inputs) {
            return refuse(
                AcquisitionError::QuotaExhausted { reset_ms },
                cache,
                fields,
                notes,
            );
        }
        refuse(AcquisitionError::ExtractionIncomplete, cache, fields, notes)
    }
}

fn strongest_mechanism_error(capabilities: &ConnectorCapabilities) -> AcquisitionError {
    for mechanism in MECHANISM_PRECEDENCE {
        if capabilities.mechanism_level(mechanism) != CapabilityLevel::None {
            return match mechanism {
                AcquisitionMechanism::OfficialApi => AcquisitionError::ApiUnavailable,
                AcquisitionMechanism::DirectHttp => AcquisitionError::EgressUnavailable,
                AcquisitionMechanism::BrowserNetwork
                | AcquisitionMechanism::EmbeddedState
                | AcquisitionMechanism::Dom => AcquisitionError::BrowserUnavailable,
            };
        }
    }
    AcquisitionError::ExtractionIncomplete
}

fn plan(
    decision: PlanDecision,
    cache: CacheCoverage,
    fields: Vec<RequestedField>,
    notes: Vec<PlanNote>,
) -> AcquisitionPlan {
    AcquisitionPlan {
        decision,
        cache,
        fields,
        notes,
    }
}

fn refuse(
    error: AcquisitionError,
    cache: CacheCoverage,
    fields: Vec<RequestedField>,
    notes: Vec<PlanNote>,
) -> AcquisitionPlan {
    plan(PlanDecision::Refuse { error }, cache, fields, notes)
}
