//! Health suite: connector degradation/cooldown/recovery and per-field
//! extraction-strategy memory, observed through the context and enforced by
//! the planner.

use std::sync::Arc;

use faktor_provider::egress::EgressError;

use crate::cache::{CacheState, FreshnessClassTtl};
use crate::capability::{ConnectorCapabilities, CredentialAvailability};
use crate::error::{AcquisitionError, VerificationKind};
use crate::health::{
    ConnectorHealth, ExtractionHealth, ExtractionOutcome, HealthPolicy, HealthTracker,
};
use crate::http::{fetch_direct_http, HttpFetch};
use crate::mechanism::AcquisitionMechanism;
use crate::planner::{AcquisitionPlan, AcquisitionPlanner, PlanDecision, PlanNote, PlannerInputs};
use crate::quota::{QuotaState, QuotaWindow};
use crate::request::{AcquisitionRequest, RequestedField, RequestedFields, RequestedFreshness};
use crate::tests::{ctx_for, ScriptedResponse, ScriptedTransport, TEST_URL};

const NOW: u64 = 1_000_000;

fn plan_with(
    health: &ConnectorHealth,
    field_health: &ExtractionHealth,
    now_ms: u64,
) -> AcquisitionPlan {
    let request = AcquisitionRequest::new(
        TEST_URL,
        RequestedFields::of([RequestedField::Identity, RequestedField::Availability]),
        RequestedFreshness::Live,
    )
    .unwrap();
    let capabilities = ConnectorCapabilities::none().with_full(AcquisitionMechanism::DirectHttp);
    let credentials = CredentialAvailability::none();
    let quota = QuotaState::new(QuotaWindow::per_minute(1_000), now_ms);
    let cache = CacheState::new();
    let ttl = FreshnessClassTtl::default();
    AcquisitionPlanner::default().plan(&PlannerInputs {
        request: &request,
        capabilities: &capabilities,
        health,
        credentials: &credentials,
        quota: &quota,
        cache: &cache,
        ttl: &ttl,
        field_health,
        now_ms,
    })
}

fn planner_health(health: &ConnectorHealth, now_ms: u64) -> PlanDecision {
    plan_with(health, &ExtractionHealth::new(), now_ms).decision
}

#[tokio::test]
async fn transient_failures_degrade_then_cool_down_and_recover_by_time() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::json(r#"{"ok":true}"#),
    ]));
    let (ctx, clock) = ctx_for(transport);
    let ctx = ctx.with_health(HealthTracker::new(HealthPolicy {
        degrade_threshold: 2,
        cooldown_ms: 500,
    }));

    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(ctx.health().as_str(), "degraded");
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        ctx.health(),
        ConnectorHealth::CoolingDown {
            until_ms: NOW + 500
        }
    );

    // The planner refuses during the cooldown and admits the path again the
    // instant the cooldown expires — time alone recovers it.
    assert!(matches!(
        planner_health(&ctx.health(), NOW + 499),
        PlanDecision::Refuse {
            error: AcquisitionError::CoolingDown { .. }
        }
    ));
    assert!(matches!(
        planner_health(&ctx.health(), NOW + 500),
        PlanDecision::Acquire { .. }
    ));

    // A success at that instant clears the state and restores throughput.
    clock.set(NOW as i64 + 500);
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    assert_eq!(ctx.health(), ConnectorHealth::Healthy);
}

#[tokio::test]
async fn a_success_clears_degradation_and_resets_the_transient_counter() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::failure(EgressError::Transport("reset".into())),
        ScriptedResponse::json(r#"{"ok":true}"#),
    ]));
    let (ctx, _clock) = ctx_for(transport);
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert!(ctx.health().is_degraded());
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap();
    assert_eq!(ctx.health(), ConnectorHealth::Healthy);
}

#[tokio::test]
async fn rate_limit_and_verification_states_come_from_honest_responses() {
    let transport = Arc::new(ScriptedTransport::new([
        ScriptedResponse::new(429, "").header("retry-after", "5")
    ]));
    let (ctx, _clock) = ctx_for(transport);
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        ctx.health(),
        ConnectorHealth::RateLimited {
            until_ms: NOW + 5_000
        }
    );
    // Before the window: typed refusal; after: usable again.
    assert!(matches!(
        planner_health(&ctx.health(), NOW + 1),
        PlanDecision::Refuse {
            error: AcquisitionError::RateLimited { .. }
        }
    ));
    assert!(matches!(
        planner_health(&ctx.health(), NOW + 5_000),
        PlanDecision::Acquire { .. }
    ));

    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::new(403, "")]));
    let (ctx, _clock) = ctx_for(transport);
    fetch_direct_http(&ctx, &HttpFetch::get(TEST_URL))
        .await
        .unwrap_err();
    assert_eq!(
        ctx.health().verification_kind(),
        Some(VerificationKind::Challenge)
    );
}

#[test]
fn human_and_quota_states_never_recover_by_time_alone() {
    // Verification demands a human forever until explicitly cleared.
    assert_eq!(
        planner_health(
            &ConnectorHealth::VerificationRequired {
                kind: VerificationKind::Challenge
            },
            u64::MAX,
        ),
        PlanDecision::RequireVerification {
            kind: VerificationKind::Challenge
        }
    );
    assert_eq!(
        planner_health(&ConnectorHealth::AuthenticationRequired, u64::MAX),
        PlanDecision::Refuse {
            error: AcquisitionError::AuthenticationRequired
        }
    );
    assert_eq!(
        planner_health(&ConnectorHealth::Unavailable, u64::MAX),
        PlanDecision::Refuse {
            error: AcquisitionError::EgressUnavailable
        }
    );

    // Quota exhaustion is time-based: refusal until the reset, then usable.
    let quota_health = ConnectorHealth::QuotaExhausted {
        reset_ms: NOW + 1_000,
    };
    assert_eq!(
        planner_health(&quota_health, NOW),
        PlanDecision::Refuse {
            error: AcquisitionError::QuotaExhausted {
                reset_ms: NOW + 1_000
            }
        }
    );
    assert!(matches!(
        planner_health(&quota_health, NOW + 1_000),
        PlanDecision::Acquire { .. }
    ));
}

#[test]
fn per_field_strategy_memory_remembers_the_best_strategy() {
    let mut health = ExtractionHealth::new();
    // DirectHttp drifted on availability and never succeeded: proven bad.
    health.record(
        AcquisitionMechanism::DirectHttp,
        RequestedField::Availability,
        ExtractionOutcome::SchemaMismatch,
        NOW,
    );
    health.record(
        AcquisitionMechanism::DirectHttp,
        RequestedField::Availability,
        ExtractionOutcome::Conflict,
        NOW,
    );
    // BrowserNetwork succeeded there before: the remembered best.
    health.record(
        AcquisitionMechanism::BrowserNetwork,
        RequestedField::Availability,
        ExtractionOutcome::Success,
        NOW,
    );
    assert!(health
        .record_of(
            AcquisitionMechanism::DirectHttp,
            RequestedField::Availability
        )
        .unwrap()
        .is_proven_bad());
    assert_eq!(
        health.best_strategy(RequestedField::Availability),
        Some(AcquisitionMechanism::BrowserNetwork)
    );
    assert_eq!(
        health.ranked_strategies(RequestedField::Availability),
        vec![AcquisitionMechanism::BrowserNetwork]
    );
    // Not-applicable is recorded but never selected as a best strategy.
    health.record(
        AcquisitionMechanism::EmbeddedState,
        RequestedField::Availability,
        ExtractionOutcome::NotApplicable,
        NOW,
    );
    assert_eq!(
        health.best_strategy(RequestedField::Availability),
        Some(AcquisitionMechanism::BrowserNetwork)
    );

    // The planner consumes the memory: the proven-bad strategy is skipped
    // for the field it never served, and the note names both facts.
    let request = AcquisitionRequest::new(
        TEST_URL,
        RequestedFields::of([RequestedField::Availability]),
        RequestedFreshness::Live,
    )
    .unwrap();
    let capabilities = ConnectorCapabilities::none()
        .with_full(AcquisitionMechanism::DirectHttp)
        .with_full(AcquisitionMechanism::BrowserNetwork);
    let credentials = CredentialAvailability::none();
    let quota = QuotaState::new(QuotaWindow::per_minute(100), NOW);
    let cache = CacheState::new();
    let ttl = FreshnessClassTtl::default();
    let plan = AcquisitionPlanner::default().plan(&PlannerInputs {
        request: &request,
        capabilities: &capabilities,
        health: &ConnectorHealth::Healthy,
        credentials: &credentials,
        quota: &quota,
        cache: &cache,
        ttl: &ttl,
        field_health: &health,
        now_ms: NOW,
    });
    match plan.decision {
        PlanDecision::Acquire { mechanism, .. } => {
            assert_eq!(mechanism, AcquisitionMechanism::BrowserNetwork);
        }
        other => panic!("expected an acquire decision, got {other:?}"),
    }
    assert!(plan.notes.contains(&PlanNote::ProvenBadStrategy {
        mechanism: AcquisitionMechanism::DirectHttp
    }));
    assert!(plan.notes.contains(&PlanNote::RememberedBestStrategy {
        field: RequestedField::Availability
    }));
}

#[tokio::test]
async fn an_unavailable_browser_path_stays_unusable_until_cleared() {
    let transport = Arc::new(ScriptedTransport::new([ScriptedResponse::json("{}")]));
    let (ctx, _clock) = ctx_for(transport);
    ctx.report_error(&AcquisitionError::BrowserUnavailable);
    assert_eq!(ctx.health(), ConnectorHealth::Unavailable);
    // Even after an unbounded wait the state stands until cleared.
    assert!(!ctx.health().usable_at(u64::MAX));
    ctx.set_health(ConnectorHealth::Healthy);
    assert!(ctx.health().usable_at(u64::MAX));
}
