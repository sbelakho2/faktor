//! Planner suite: deterministic precedence, cache/freshness policy,
//! substitution policy, health and per-field strategy memory.
//!
//! Every scenario here is a table row: given exactly the planner's documented
//! inputs, the plan must be reproducible byte-for-byte and must never consult
//! a site name (the scan suite enforces the absence of site knowledge).

use crate::cache::{CacheEntry, CacheKey, CacheState, FreshnessClassTtl};
use crate::capability::{
    CapabilityLevel, ConnectorCapabilities, CredentialAvailability, CredentialStatus,
};
use crate::error::{AcquisitionError, VerificationKind};
use crate::health::{ConnectorHealth, ExtractionHealth, ExtractionOutcome};
use crate::mechanism::AcquisitionMechanism;
use crate::planner::{
    AcquisitionPlan, AcquisitionPlanner, PlanDecision, PlanNote, PlannerInputs, SubstitutionPolicy,
};
use crate::quota::{QuotaState, QuotaWindow};
use crate::request::{AcquisitionRequest, RequestedField, RequestedFields, RequestedFreshness};

const NOW: u64 = 1_000_000;
const IDENTITY: &str = "https://example.invalid/data";

fn request(freshness: RequestedFreshness) -> AcquisitionRequest {
    AcquisitionRequest::new(
        IDENTITY,
        RequestedFields::of([RequestedField::Identity, RequestedField::Availability]),
        freshness,
    )
    .unwrap()
}

struct Fixture {
    request: AcquisitionRequest,
    capabilities: ConnectorCapabilities,
    health: ConnectorHealth,
    credentials: CredentialAvailability,
    quota: QuotaState,
    cache: CacheState,
    ttl: FreshnessClassTtl,
    field_health: ExtractionHealth,
    substitution: SubstitutionPolicy,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            request: request(RequestedFreshness::Live),
            capabilities: ConnectorCapabilities::none(),
            health: ConnectorHealth::Healthy,
            credentials: CredentialAvailability::none(),
            quota: QuotaState::new(QuotaWindow::per_minute(1_000), NOW),
            cache: CacheState::new(),
            ttl: FreshnessClassTtl::default(),
            field_health: ExtractionHealth::new(),
            substitution: SubstitutionPolicy::default(),
        }
    }
}

impl Fixture {
    fn new() -> Self {
        Self::default()
    }

    fn plan(&self) -> AcquisitionPlan {
        AcquisitionPlanner::new(self.substitution).plan(&PlannerInputs {
            request: &self.request,
            capabilities: &self.capabilities,
            health: &self.health,
            credentials: &self.credentials,
            quota: &self.quota,
            cache: &self.cache,
            ttl: &self.ttl,
            field_health: &self.field_health,
            now_ms: NOW,
        })
    }

    fn exhausted_quota() -> QuotaState {
        let mut quota = QuotaState::new(QuotaWindow::per_minute(1), NOW);
        quota.try_acquire(NOW).unwrap();
        assert!(quota.is_exhausted_at(NOW));
        quota
    }

    fn cached(&mut self, field: RequestedField, observed_at_ms: u64, etag: Option<&str>) {
        let class = crate::cache::freshness_class(field);
        self.cache.insert(
            CacheKey::new(IDENTITY, None, field),
            CacheEntry {
                observed_at_ms,
                class,
                etag: etag.map(str::to_string),
                last_modified: None,
                content_digest: Some("digest".into()),
                content: None,
            },
        );
    }

    fn acquire(
        &self,
    ) -> (
        AcquisitionMechanism,
        CapabilityLevel,
        Option<ConditionalExpectation>,
    ) {
        match &self.plan().decision {
            PlanDecision::Acquire {
                mechanism,
                coverage,
                conditional,
                ..
            } => (
                *mechanism,
                *coverage,
                conditional
                    .as_ref()
                    .map(|validators| ConditionalExpectation {
                        etag: validators.etag.clone(),
                        last_modified: validators.last_modified.clone(),
                    }),
            ),
            other => panic!("expected an acquire decision, got {other:?}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ConditionalExpectation {
    etag: Option<String>,
    last_modified: Option<String>,
}

#[test]
fn mechanism_precedence_table_is_the_spec_order() {
    let scenarios: Vec<(Vec<AcquisitionMechanism>, AcquisitionMechanism)> = vec![
        (
            vec![
                AcquisitionMechanism::OfficialApi,
                AcquisitionMechanism::DirectHttp,
                AcquisitionMechanism::BrowserNetwork,
                AcquisitionMechanism::EmbeddedState,
                AcquisitionMechanism::Dom,
            ],
            AcquisitionMechanism::OfficialApi,
        ),
        (
            vec![
                AcquisitionMechanism::DirectHttp,
                AcquisitionMechanism::BrowserNetwork,
                AcquisitionMechanism::EmbeddedState,
                AcquisitionMechanism::Dom,
            ],
            AcquisitionMechanism::DirectHttp,
        ),
        (
            vec![
                AcquisitionMechanism::BrowserNetwork,
                AcquisitionMechanism::EmbeddedState,
                AcquisitionMechanism::Dom,
            ],
            AcquisitionMechanism::BrowserNetwork,
        ),
        (
            vec![
                AcquisitionMechanism::EmbeddedState,
                AcquisitionMechanism::Dom,
            ],
            AcquisitionMechanism::EmbeddedState,
        ),
        (vec![AcquisitionMechanism::Dom], AcquisitionMechanism::Dom),
    ];
    for (available, expected) in scenarios {
        let mut fixture = Fixture::new();
        for mechanism in &available {
            fixture.capabilities = fixture.capabilities.with_full(*mechanism);
        }
        let (mechanism, coverage, _) = fixture.acquire();
        assert_eq!(
            mechanism, expected,
            "available {available:?} must select {expected:?}"
        );
        assert_eq!(coverage, CapabilityLevel::Full);
    }
}

#[test]
fn capability_credentials_health_and_quota_each_disqualify_the_api() {
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::OfficialApi)
        .with_full(AcquisitionMechanism::DirectHttp);
    assert_eq!(fixture.acquire().0, AcquisitionMechanism::OfficialApi);

    // A field the API cannot serve at all disqualifies it, with a note.
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::OfficialApi)
        .with_field(
            AcquisitionMechanism::OfficialApi,
            RequestedField::Availability,
            CapabilityLevel::None,
        )
        .with_full(AcquisitionMechanism::DirectHttp);
    let plan = fixture.plan();
    assert_eq!(fixture.acquire().0, AcquisitionMechanism::DirectHttp);
    assert!(plan.notes.contains(&PlanNote::FieldsUnsupported {
        mechanism: AcquisitionMechanism::OfficialApi,
        field: RequestedField::Availability,
    }));

    // Unusable credentials disqualify it.
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::OfficialApi)
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.credentials = CredentialAvailability::none()
        .with_status(AcquisitionMechanism::OfficialApi, CredentialStatus::Expired);
    assert_eq!(fixture.acquire().0, AcquisitionMechanism::DirectHttp);

    // A blocked health state refuses the whole connector path (health is a
    // property of the connector, not of one mechanism), and recovers by time
    // alone once the window passes.
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::OfficialApi)
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.health = ConnectorHealth::RateLimited {
        until_ms: NOW + 60_000,
    };
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::Refuse {
            error: AcquisitionError::RateLimited {
                retry_after_ms: 60_000
            }
        }
    );
    fixture.health = ConnectorHealth::RateLimited { until_ms: NOW - 1 };
    assert_eq!(fixture.acquire().0, AcquisitionMechanism::OfficialApi);
}

#[test]
fn exhausted_api_quota_refuses_by_default_and_substitutes_only_on_opt_in() {
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::OfficialApi)
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.quota = Fixture::exhausted_quota();

    let reset_ms = NOW + 60_000;
    // Default: never silently circumvent the official API limit.
    let plan = fixture.plan();
    assert_eq!(
        plan.decision,
        PlanDecision::Refuse {
            error: AcquisitionError::QuotaExhausted { reset_ms }
        }
    );
    assert!(plan
        .notes
        .contains(&PlanNote::ApiQuotaExhausted { reset_ms }));
    assert!(plan
        .notes
        .contains(&PlanNote::ApiQuotaExhaustedNoSubstitution));

    // Explicit opt-in: substitute, and record that it was a substitution.
    fixture.substitution = SubstitutionPolicy {
        allow_non_api_when_api_quota_exhausted: true,
    };
    let plan = fixture.plan();
    match plan.decision {
        PlanDecision::Acquire {
            mechanism,
            substituted,
            ..
        } => {
            assert_eq!(mechanism, AcquisitionMechanism::DirectHttp);
            assert!(substituted);
        }
        other => panic!("expected a substituted acquire, got {other:?}"),
    }
    assert!(plan.notes.contains(&PlanNote::SubstitutedMechanism {
        from: AcquisitionMechanism::OfficialApi,
        to: AcquisitionMechanism::DirectHttp,
    }));

    // A connector that advertises no official API cannot substitute one.
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.quota = Fixture::exhausted_quota();
    fixture.substitution = SubstitutionPolicy {
        allow_non_api_when_api_quota_exhausted: true,
    };
    match fixture.plan().decision {
        PlanDecision::Acquire {
            mechanism,
            substituted,
            ..
        } => {
            assert_eq!(mechanism, AcquisitionMechanism::DirectHttp);
            assert!(!substituted);
        }
        other => panic!("expected a plain acquire, got {other:?}"),
    }
}

#[test]
fn api_quota_exhaustion_from_health_recovers_when_the_reset_passes() {
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::OfficialApi)
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.health = ConnectorHealth::QuotaExhausted { reset_ms: NOW + 10 };
    let plan = fixture.plan();
    assert_eq!(
        plan.decision,
        PlanDecision::Refuse {
            error: AcquisitionError::QuotaExhausted { reset_ms: NOW + 10 }
        }
    );
    // After the reset instant the API is usable again with no extra state.
    fixture.health = ConnectorHealth::QuotaExhausted { reset_ms: NOW - 1 };
    assert_eq!(fixture.acquire().0, AcquisitionMechanism::OfficialApi);
}

#[test]
fn fresh_cache_serves_prefer_cache_but_never_a_live_request() {
    let mut fixture = Fixture::new();
    fixture.request = request(RequestedFreshness::PreferCache);
    fixture.cached(RequestedField::Identity, NOW, None);
    fixture.cached(RequestedField::Availability, NOW, None);
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::ServeFromCache { stale: false }
    );

    // A `Live` request is never served from cache, however fresh.
    fixture.request = request(RequestedFreshness::Live);
    fixture.capabilities = fixture.capabilities.with_full(AcquisitionMechanism::Dom);
    assert_eq!(fixture.acquire().0, AcquisitionMechanism::Dom);
}

#[test]
fn cache_only_serves_stale_flagged_or_refuses_never_live() {
    let mut fixture = Fixture::new();
    fixture.request = request(RequestedFreshness::CacheOnly);
    fixture.capabilities = ConnectorCapabilities::none();
    fixture.ttl = FreshnessClassTtl {
        slow_ms: 1,
        moderate_ms: 1,
        fast_ms: 1,
    };

    // Nothing cached: typed refusal, no mechanism consulted.
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::Refuse {
            error: AcquisitionError::ExtractionIncomplete
        }
    );

    // A stale entry is served but flagged stale: stale is never current.
    fixture.cached(RequestedField::Identity, NOW - 10, None);
    fixture.cached(RequestedField::Availability, NOW - 10, None);
    let plan = fixture.plan();
    assert_eq!(plan.decision, PlanDecision::ServeFromCache { stale: true });
    assert!(plan.notes.contains(&PlanNote::StaleCacheServed));
}

#[test]
fn conditional_revalidation_requires_every_field_present() {
    let mut fixture = Fixture::new();
    fixture.request = request(RequestedFreshness::PreferCache);
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.ttl = FreshnessClassTtl {
        slow_ms: 1,
        moderate_ms: 1,
        fast_ms: 1,
    };
    fixture.cached(RequestedField::Identity, NOW - 10, Some("\"v1\""));
    fixture.cached(RequestedField::Availability, NOW - 10, Some("\"v1\""));
    let plan = fixture.plan();
    assert_eq!(
        fixture.acquire().2,
        Some(ConditionalExpectation {
            etag: Some("\"v1\"".into()),
            last_modified: None
        })
    );
    assert!(plan.notes.contains(&PlanNote::ConditionalRevalidation));

    // One missing field: a 304 would leave a hole, so no conditional request.
    let mut fixture = Fixture::new();
    fixture.request = request(RequestedFreshness::PreferCache);
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.ttl = FreshnessClassTtl {
        slow_ms: 1,
        moderate_ms: 1,
        fast_ms: 1,
    };
    fixture.cached(RequestedField::Identity, NOW - 10, Some("\"v1\""));
    assert_eq!(fixture.acquire().2, None);
}

#[test]
fn degraded_health_is_usable_but_flagged() {
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.health = ConnectorHealth::Degraded {
        reason: "network_timeout".into(),
    };
    let plan = fixture.plan();
    match plan.decision {
        PlanDecision::Acquire { degraded, .. } => assert!(degraded),
        other => panic!("expected acquire, got {other:?}"),
    }
    assert!(plan.notes.contains(&PlanNote::HealthDegraded {
        reason: "degraded".into()
    }));
}

#[test]
fn human_and_unavailable_states_refuse_or_demand_verification() {
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::DirectHttp);
    fixture.health = ConnectorHealth::VerificationRequired {
        kind: VerificationKind::Challenge,
    };
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::RequireVerification {
            kind: VerificationKind::Challenge
        }
    );

    fixture.health = ConnectorHealth::AuthenticationRequired;
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::Refuse {
            error: AcquisitionError::AuthenticationRequired
        }
    );

    fixture.health = ConnectorHealth::Unavailable;
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::Refuse {
            error: AcquisitionError::EgressUnavailable
        }
    );

    fixture.health = ConnectorHealth::CoolingDown { until_ms: NOW + 5 };
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::Refuse {
            error: AcquisitionError::CoolingDown { until_ms: NOW + 5 }
        }
    );
    fixture.health = ConnectorHealth::RateLimited { until_ms: NOW + 5 };
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::Refuse {
            error: AcquisitionError::RateLimited { retry_after_ms: 5 }
        }
    );
}

#[test]
fn per_field_strategy_memory_skips_proven_bad_mechanisms() {
    let mut fixture = Fixture::new();
    // Only the field the strategy never served: a mechanism proven bad for
    // one field is not skipped for fields it can still serve.
    fixture.request = AcquisitionRequest::new(
        IDENTITY,
        RequestedFields::of([RequestedField::Availability]),
        RequestedFreshness::Live,
    )
    .unwrap();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::DirectHttp)
        .with_full(AcquisitionMechanism::BrowserNetwork);
    fixture.field_health.record(
        AcquisitionMechanism::DirectHttp,
        RequestedField::Availability,
        ExtractionOutcome::SchemaMismatch,
        NOW,
    );
    fixture.field_health.record(
        AcquisitionMechanism::BrowserNetwork,
        RequestedField::Availability,
        ExtractionOutcome::Success,
        NOW,
    );
    let plan = fixture.plan();
    assert_eq!(fixture.acquire().0, AcquisitionMechanism::BrowserNetwork);
    assert!(plan.notes.contains(&PlanNote::ProvenBadStrategy {
        mechanism: AcquisitionMechanism::DirectHttp
    }));
    assert!(plan.notes.contains(&PlanNote::RememberedBestStrategy {
        field: RequestedField::Availability
    }));
}

#[test]
fn plans_are_deterministic_byte_for_byte() {
    let mut fixture = Fixture::new();
    fixture.capabilities = fixture
        .capabilities
        .with_full(AcquisitionMechanism::DirectHttp)
        .with_field(
            AcquisitionMechanism::DirectHttp,
            RequestedField::BulkSet,
            CapabilityLevel::Partial,
        );
    fixture.health = ConnectorHealth::Degraded {
        reason: "network_timeout".into(),
    };
    fixture.request =
        AcquisitionRequest::new(IDENTITY, RequestedFields::all(), RequestedFreshness::Live)
            .unwrap();
    let first = fixture.plan();
    let second = fixture.plan();
    assert_eq!(first, second);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}

#[test]
fn empty_field_sets_and_invalid_identities_are_typed_refusals() {
    let mut fixture = Fixture::new();
    fixture.request =
        AcquisitionRequest::new(IDENTITY, RequestedFields::none(), RequestedFreshness::Live)
            .unwrap();
    assert_eq!(
        fixture.plan().decision,
        PlanDecision::Refuse {
            error: AcquisitionError::InvalidRequest {
                detail: "requested fields are empty".into()
            }
        }
    );

    let mut fixture = Fixture::new();
    fixture.request.identity = "   ".into();
    match fixture.plan().decision {
        PlanDecision::Refuse {
            error: AcquisitionError::InvalidRequest { .. },
        } => {}
        other => panic!("expected an invalid-request refusal, got {other:?}"),
    }
}

#[test]
fn partial_coverage_is_selected_but_flagged() {
    let mut fixture = Fixture::new();
    fixture.capabilities = ConnectorCapabilities::none()
        .with_mechanism(AcquisitionMechanism::Dom, CapabilityLevel::Partial);
    let plan = fixture.plan();
    let (mechanism, coverage, _) = fixture.acquire();
    assert_eq!(mechanism, AcquisitionMechanism::Dom);
    assert_eq!(coverage, CapabilityLevel::Partial);
    assert!(plan.notes.contains(&PlanNote::PartialCoverage {
        mechanism: AcquisitionMechanism::Dom
    }));
}
