//! quality_authority_tests (mechanically split from `lib`).

use super::*;
use crate::tests::{desc, econ};
use faktor_core::model::{MicroUsdPerMillionTokens, PriceQuote, QualityMetric};

fn perf(code: u8, context: u8) -> ModelPerformance {
    ModelPerformance {
        context_reliability: context,
        coding_reliability: code,
        estimated_latency_ms: 500,
        rate_limit_state: RateLimitState::Healthy,
    }
}

fn descriptor(provider: &str, model: &str, code: u8, context: u8) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.into(),
        model: model.into(),
        context: 128_000,
        max_output: 16_000,
        tools: true,
        parallel_tools: true,
        reasoning: true,
        thinking: true,
        vision: false,
        structured_output: true,
        embeddings: false,
        streaming: true,
        economics: ModelEconomics {
            tool_reliability: code,
            reasoning_reliability: code,
            coding_reliability: code,
            context_reliability: context,
            estimated_latency_ms: 500,
            ..Default::default()
        },
        source: faktor_core::model::ModelSource::ProviderCatalog,
    }
}

fn exact(input: u64, output: u64) -> PricingState {
    PricingState::Known(PricingSnapshot::exact(
        PriceQuote {
            input: MicroUsdPerMillionTokens(input),
            output: MicroUsdPerMillionTokens(output),
            cache_read: MicroUsdPerMillionTokens(0),
            cache_write: MicroUsdPerMillionTokens(0),
        },
        1,
        "test-row".into(),
    ))
}

fn candidate(
    provider: &str,
    model: &str,
    code: u8,
    context: u8,
    quality: QualityStatement,
) -> RouteCandidate {
    RouteCandidate::with_quality(
        descriptor(provider, model, code, context),
        exact(1_000, 2_000),
        quality,
    )
}

fn request(floor: u8) -> RouteRequest {
    RouteRequest {
        phase: RouterPhase::Implement,
        required_capabilities: vec![],
        context_tokens: 1_000,
        estimated_output_tokens: 100,
        quality_floor: floor,
        quality_target: None,
        task_budget_remaining_micro: 0,
        latency_preference_ms: None,
        task_class: TaskClass::Medium,
        risk_bucket: RiskBucket::Low,
    }
}

/// A user-declared metric (UserConfigured, never measured).
fn user_metric(value: u8) -> QualityMetric {
    QualityMetric::new(
        value,
        QualityAuthority::UserConfigured {
            source: "providers.declared.quality".into(),
            version: "faktor-user-quality-v1".into(),
        },
    )
}

/// A statement declaring ALL FOUR dimensions at the same value.
fn all_user(value: u8) -> QualityStatement {
    QualityStatement {
        tool: user_metric(value),
        reasoning: user_metric(value),
        coding: user_metric(value),
        context: user_metric(value),
    }
}

#[test]
fn conservative_unknown_placeholder_never_clears_a_floor_but_a_declaration_does() {
    // The audit core (item 11): the undeclared row carries the same
    // numeric 50 as the declared one, but its authority says UNKNOWN —
    // so it clears no positive floor. The user declaration authorizes
    // its own value; both are admitted at floor 0 (no requirement).
    let unknown = candidate(
        "unknown",
        "m",
        50,
        50,
        QualityStatement::conservative_unknown(&perf(50, 50)),
    );
    let declared = candidate("declared", "m", 50, 50, all_user(50));
    let both = vec![unknown.clone(), declared.clone()];
    let q = qualified_priced_candidates_at(
        &both,
        &request(50),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(q.len(), 1, "only the declared row is authorized");
    assert_eq!(q[0].descriptor.provider, "declared");
    let q0 = qualified_priced_candidates_at(
        &both,
        &request(0),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(q0.len(), 2, "floor 0 imposes no quality requirement");
    // The statement itself states the semantics exactly.
    assert!(!unknown.quality.clears_floor(RouterPhase::Implement, 50));
    assert!(declared.quality.clears_floor(RouterPhase::Implement, 50));
}

#[test]
fn per_dimension_authority_decides_each_phase_by_its_own_metric() {
    // Item 1 adversarial at qualification: a coding-only declaration
    // authorizes the hard Implement floor but can never authorize
    // Compact's CONTEXT floor (the context metric stays
    // ConservativeUnknown even though it sits in the same statement).
    let coding_only = candidate(
        "coding-only",
        "m",
        80,
        50,
        QualityStatement {
            coding: user_metric(80),
            ..QualityStatement::default()
        },
    );
    let implement = qualified_priced_candidates_at(
        std::slice::from_ref(&coding_only),
        &request(70),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(
        implement.len(),
        1,
        "coding 80 authorizes the implement floor"
    );
    let compact_req = RouteRequest {
        phase: RouterPhase::Compact,
        quality_floor: 60,
        ..request(60)
    };
    let err = qualified_priced_candidates_at(
        std::slice::from_ref(&coding_only),
        &compact_req,
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap_err();
    assert_eq!(
        err.quality_survivors, 0,
        "a coding declaration must not authorize the compaction context floor"
    );
    // The mirror: context-only authorizes Compact, not Implement.
    let context_only = candidate(
        "context-only",
        "m",
        50,
        80,
        QualityStatement {
            context: user_metric(80),
            ..QualityStatement::default()
        },
    );
    let compact = qualified_priced_candidates_at(
        std::slice::from_ref(&context_only),
        &compact_req,
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(compact.len(), 1, "context 80 authorizes the compact floor");
    let err = qualified_priced_candidates_at(
        std::slice::from_ref(&context_only),
        &request(70),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap_err();
    assert_eq!(
        err.quality_survivors, 0,
        "a context declaration must not authorize the implement coding floor"
    );
    // All-four still works on both phases.
    let all = candidate("all", "m", 80, 80, all_user(80));
    assert_eq!(
        qualified_priced_candidates_at(
            std::slice::from_ref(&all),
            &request(70),
            &[],
            &LiveHealth::default(),
            unix_now_ms(),
        )
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        qualified_priced_candidates_at(
            std::slice::from_ref(&all),
            &compact_req,
            &[],
            &LiveHealth::default(),
            unix_now_ms(),
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn mixed_declaration_judges_each_phase_from_its_own_metric() {
    // Item 1 adversarial: a mixed statement (coding declared 40,
    // context declared 90) passes or fails purely on the phase's own
    // metric value+authority — never on a global statement authority.
    let mixed = candidate(
        "mixed",
        "m",
        40,
        90,
        QualityStatement {
            coding: user_metric(40),
            context: user_metric(90),
            ..QualityStatement::default()
        },
    );
    assert_eq!(
        qualified_priced_candidates_at(
            std::slice::from_ref(&mixed),
            &request(40),
            &[],
            &LiveHealth::default(),
            unix_now_ms(),
        )
        .unwrap()
        .len(),
        1
    );
    let err = qualified_priced_candidates_at(
        std::slice::from_ref(&mixed),
        &request(41),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap_err();
    assert_eq!(err.quality_survivors, 0, "implement reads coding 40");
    let compact_req = RouteRequest {
        phase: RouterPhase::Compact,
        quality_floor: 90,
        ..request(90)
    };
    assert_eq!(
        qualified_priced_candidates_at(
            std::slice::from_ref(&mixed),
            &compact_req,
            &[],
            &LiveHealth::default(),
            unix_now_ms(),
        )
        .unwrap()
        .len(),
        1
    );
    let compact_high = RouteRequest {
        phase: RouterPhase::Compact,
        quality_floor: 91,
        ..compact_req
    };
    let err = qualified_priced_candidates_at(
        std::slice::from_ref(&mixed),
        &compact_high,
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap_err();
    assert_eq!(err.quality_survivors, 0, "compact reads context 90");
}

#[test]
fn measured_outcomes_supersede_declared_quality_in_both_directions() {
    // Declared 95 with a recorded verified FAILURE: the measured
    // statement supersedes the declaration and floor 80 refuses it.
    let declared = candidate("declared", "m", 95, 95, all_user(95));
    let key = OutcomeKey {
        provider: "declared".into(),
        model: "m".into(),
        phase: RouterPhase::Implement,
        task_class: TaskClass::Medium,
        risk_bucket: RiskBucket::Low,
    };
    let weak = MemoryOutcomeStore::default();
    weak.append_sample(
        &key,
        OutcomeSample {
            verified_success: false,
            rework_cost_micro: 500_000,
            rework_turns: 2,
        },
    );
    let weak_rows = vec![declared.clone()];
    let err = qualified_priced_candidates_authoritative_at(
        &weak_rows,
        &request(80),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
        &weak,
    )
    .unwrap_err();
    assert_eq!(err.quality_survivors, 0, "measured failure supersedes 95");
    // An UNDECLARED placeholder with strong durable evidence is
    // authorized BY THE MEASUREMENT (never by its numeric 50).
    let unknown = candidate(
        "unknown",
        "m",
        50,
        50,
        QualityStatement::conservative_unknown(&perf(50, 50)),
    );
    let strong = MemoryOutcomeStore::default();
    let key = OutcomeKey {
        provider: "unknown".into(),
        model: "m".into(),
        ..key
    };
    for _ in 0..100 {
        strong.append_sample(
            &key,
            OutcomeSample {
                verified_success: true,
                rework_cost_micro: 0,
                rework_turns: 0,
            },
        );
    }
    let unknown_rows = vec![unknown.clone()];
    let q = qualified_priced_candidates_authoritative_at(
        &unknown_rows,
        &request(60),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
        &strong,
    )
    .unwrap();
    assert_eq!(q.len(), 1, "measured evidence authorizes the unknown row");
    let st = effective_quality_statement(
        &unknown,
        RouterPhase::Implement,
        TaskClass::Medium,
        RiskBucket::Low,
        unix_now_ms(),
        &strong,
    );
    for (name, metric) in [
        ("tool", &st.tool),
        ("reasoning", &st.reasoning),
        ("coding", &st.coding),
        ("context", &st.context),
    ] {
        assert!(
            metric.is_measured(),
            "{name} carries the measured authority"
        );
        assert_eq!(metric.value, st.coding.value, "{name} scaling");
    }
    assert!(st.coding.value >= 60, "confidence scales to 0..=100");
    let evidence = match st.coding.authority {
        QualityAuthority::Measured(ref outcome) => outcome.clone(),
        other => panic!("measured authority expected, got {other:?}"),
    };
    assert_eq!(evidence.provider, "unknown");
    assert_eq!(evidence.sample_count, 100);
    assert!(evidence.success_ppm >= 600_000);
}

#[test]
fn measured_outcomes_supersede_each_declared_metric_per_dimension() {
    // Item 1: supersession is PER METRIC. A declared statement (coding
    // 95, context 95) whose phase metric is measured at a weak
    // confidence refuses the implement floor even though the OTHER
    // declared dimensions remain high; and a declared LOW coding metric
    // is lifted by strong measurement.
    let weak_key = OutcomeKey {
        provider: "declared".into(),
        model: "m".into(),
        phase: RouterPhase::Implement,
        task_class: TaskClass::Medium,
        risk_bucket: RiskBucket::Low,
    };
    let weak = MemoryOutcomeStore::default();
    for _ in 0..50 {
        weak.append_sample(
            &weak_key,
            OutcomeSample {
                verified_success: false,
                rework_cost_micro: 100,
                rework_turns: 1,
            },
        );
    }
    let declared = candidate("declared", "m", 95, 95, all_user(95));
    let err = qualified_priced_candidates_authoritative_at(
        std::slice::from_ref(&declared),
        &request(80),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
        &weak,
    )
    .unwrap_err();
    assert_eq!(
        err.quality_survivors, 0,
        "the measured coding metric supersedes the declared 95"
    );
    // A declared-low candidate with strong measured success clears the
    // floor its declaration alone would have missed.
    let low = candidate("low", "m", 20, 20, all_user(20));
    let strong_key = OutcomeKey {
        provider: "low".into(),
        ..weak_key.clone()
    };
    let strong = MemoryOutcomeStore::default();
    for _ in 0..200 {
        strong.append_sample(
            &strong_key,
            OutcomeSample {
                verified_success: true,
                rework_cost_micro: 0,
                rework_turns: 0,
            },
        );
    }
    let q = qualified_priced_candidates_authoritative_at(
        std::slice::from_ref(&low),
        &request(80),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
        &strong,
    )
    .unwrap();
    assert_eq!(q.len(), 1, "measured success supersedes the declared 20");
    let st = effective_quality_statement(
        &low,
        RouterPhase::Implement,
        TaskClass::Medium,
        RiskBucket::Low,
        unix_now_ms(),
        &strong,
    );
    assert!(st.coding.value >= 80 && st.coding.is_measured());
}

// ==================================================================
// Adaptive target preference (audit item 2): the request carries the
// hard minimum plus an optional preferred target; target-or-higher
// candidates always win, `[minimum, target)` serves only when no
// target candidate survived every hard axis, and nothing below the
// minimum ever serves.
// ==================================================================

/// The adaptive request: hard minimum 50, preferred target 60.
fn adaptive_request(min: u8, target: u8) -> RouteRequest {
    RouteRequest {
        quality_target: Some(target),
        ..request(min)
    }
}

#[test]
fn adaptive_target_beats_a_cheaper_below_target_candidate() {
    // A declared 60 candidate that costs 300x more than the declared 50
    // one: the target tier wins outright — cost never outranks the
    // preferred target while a target candidate survives.
    let strong = candidate("strong", "m60", 60, 60, all_user(60));
    let cheap = RouteCandidate::with_quality(
        descriptor("cheap", "m50", 50, 50),
        exact(100_000, 200_000),
        all_user(50),
    );
    let rows = vec![cheap, strong];
    let req = adaptive_request(50, 60);
    let q = qualified_priced_candidates_at(&rows, &req, &[], &LiveHealth::default(), unix_now_ms())
        .unwrap();
    assert_eq!(q.len(), 1, "only the target tier survives the preference");
    assert_eq!(q[0].descriptor.model, "m60");
    let service = RouterService::with_route_candidates(rows, Arc::new(EmptyOutcomeStore));
    let decision = service.route(&req, &[]).unwrap();
    assert_eq!(decision.model, "m60", "{}", decision.reasoning);
}

#[test]
fn adaptive_relaxes_into_the_band_only_without_a_target_candidate() {
    // No 60 candidate exists: the declared-50 row serves (the relaxation
    // the adaptive request authorizes) — and a 55 row also serves.
    let fifty = candidate("only", "m50", 50, 50, all_user(50));
    let req = adaptive_request(50, 60);
    let q = qualified_priced_candidates_at(
        std::slice::from_ref(&fifty),
        &req,
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(q.len(), 1);
    let service = RouterService::with_route_candidates(vec![fifty], Arc::new(EmptyOutcomeStore));
    assert_eq!(service.route(&req, &[]).unwrap().model, "m50");
    let fifty_five = candidate("band", "m55", 55, 55, all_user(55));
    let q = qualified_priced_candidates_at(
        std::slice::from_ref(&fifty_five),
        &req,
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(q.len(), 1, "55 is inside [50, 60)");
}

#[test]
fn adaptive_never_serves_below_the_minimum() {
    let weak = candidate("weak", "m40", 40, 40, all_user(40));
    let req = adaptive_request(50, 60);
    let err = qualified_priced_candidates_at(
        std::slice::from_ref(&weak),
        &req,
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap_err();
    assert_eq!(err.quality_survivors, 0, "40 never clears the hard 50");
    assert!(err.route_error().contains("quality floor"));
    let service = RouterService::with_route_candidates(vec![weak], Arc::new(EmptyOutcomeStore));
    assert!(service.route(&req, &[]).is_err());
}

#[test]
fn a_hard_request_does_not_prefer_above_its_floor() {
    // No target: the floor is the whole requirement, so every
    // above-floor candidate stays qualified (no partition happens) and
    // the existing scoring decides.
    let strong = candidate("strong", "m90", 90, 90, all_user(90));
    let band = candidate("band", "m60", 60, 60, all_user(60));
    let rows = vec![strong.clone(), band.clone()];
    let hard = qualified_priced_candidates_at(
        &rows,
        &request(50),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(
        hard.len(),
        2,
        "a hard request keeps the whole above-floor set"
    );
    let adaptive = qualified_priced_candidates_at(
        &rows,
        &adaptive_request(50, 90),
        &[],
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(adaptive.len(), 1, "the adaptive target filters to its tier");
    assert_eq!(adaptive[0].descriptor.model, "m90");
}

#[test]
fn adaptive_relaxes_when_the_target_candidate_fails_a_hard_axis() {
    // The 60 candidate costs ~36_000 micro and cannot fit the 1_000
    // micro budget (a HARD axis), so the target tier is empty and the
    // cheap 50 candidate serves.
    let strong = RouteCandidate::with_quality(
        descriptor("strong", "m60", 60, 60),
        exact(30_000_000, 60_000_000),
        all_user(60),
    );
    let cheap = RouteCandidate::with_quality(
        descriptor("cheap", "m50", 50, 50),
        exact(100_000, 200_000),
        all_user(50),
    );
    let service =
        RouterService::with_route_candidates(vec![strong, cheap], Arc::new(EmptyOutcomeStore));
    let req = RouteRequest {
        task_budget_remaining_micro: 1_000,
        ..adaptive_request(50, 60)
    };
    let decision = service.route(&req, &[]).unwrap();
    assert_eq!(
        decision.model, "m50",
        "the target candidate failed the budget axis: {}",
        decision.reasoning
    );
    // The same candidates WITHOUT the budget cap: the target tier wins.
    let uncapped = service.route(&adaptive_request(50, 60), &[]).unwrap();
    assert_eq!(uncapped.model, "m60");
}

#[test]
fn adaptive_pinned_route_is_deterministic_and_still_gated_by_the_minimum() {
    // A pinned adaptive route: the pin relaxes into [50, 60) but is
    // never displaced by another candidate — and a pin BELOW the
    // minimum is refused like any hard floor.
    let pin = candidate("pin", "m55", 55, 55, all_user(55));
    let service = RouterService::with_pinned_route_candidates(
        vec![pin],
        "pin",
        "m55",
        Arc::new(EmptyOutcomeStore),
    );
    let req = adaptive_request(50, 60);
    let first = service.route(&req, &[]).unwrap();
    let second = service.route(&req, &[]).unwrap();
    assert_eq!(first, second, "pinned decisions are byte-deterministic");
    assert_eq!(first.model, "m55");
    let below = candidate("pin", "m40", 40, 40, all_user(40));
    let refused = RouterService::with_pinned_route_candidates(
        vec![below],
        "pin",
        "m40",
        Arc::new(EmptyOutcomeStore),
    );
    assert!(
        refused.route(&req, &[]).is_err(),
        "a pin below the hard minimum is refused"
    );
}

#[test]
fn adaptive_target_preference_holds_on_the_descriptor_path_too() {
    // The legacy Router consumes the same single qualification pass: a
    // 60 descriptor beats the cheaper 50 descriptor under an adaptive
    // request, while the hard request keeps cheapest-above-floor.
    let expensive = desc(
        "expensive",
        "m60",
        true,
        100_000,
        4096,
        econ(30, 60, 60, 60),
    );
    let cheap = desc("cheap", "m50", true, 100_000, 4096, econ(1, 2, 50, 50));
    let r = Router::new(vec![expensive, cheap]);
    let req = adaptive_request(50, 60);
    assert_eq!(r.route(&req, &[]).unwrap().model, "m60");
    assert_eq!(r.route(&request(50), &[]).unwrap().model, "m50");
}

#[test]
fn adaptive_band_clamping_is_hostile_input_safe() {
    // A target below the floor is not a relaxation backwards: it is
    // dropped (hard request), and a target above 100 clamps to 100.
    assert_eq!(quality_band(50, Some(40)), (50, None));
    assert_eq!(quality_band(50, Some(50)), (50, None));
    assert_eq!(quality_band(150, Some(255)), (100, None));
    assert_eq!(quality_band(50, Some(255)), (50, Some(100)));
    assert_eq!(quality_band(0, Some(60)), (0, Some(60)));
}
