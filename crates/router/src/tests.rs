//! tests (mechanically split from `lib`).

use super::*;
use faktor_core::model::{
    MicroUsdPerToken, ModelEconomics, ModelSource, RateLimitState, RiskBucket, TaskClass,
};

pub(crate) fn desc(
    provider: &str,
    model: &str,
    tools: bool,
    context: u64,
    out: u64,
    econ: ModelEconomics,
) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.into(),
        model: model.into(),
        context,
        max_output: out,
        tools,
        parallel_tools: true,
        reasoning: true,
        thinking: true,
        vision: false,
        structured_output: true,
        embeddings: false,
        streaming: true,
        economics: econ,
        source: ModelSource::ProviderCatalog,
    }
}

pub(crate) fn econ(input: u64, output: u64, tool: u8, code: u8) -> ModelEconomics {
    ModelEconomics {
        // Test helper arguments are microUSD per token (= dollars per
        // million tokens numerically); the typed constructor documents
        // the reading at the boundary.
        input_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(input),
        output_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(output),
        cache_read_price_per_mtok: MicroUsdPerToken::from(input / 5),
        cache_write_price_per_mtok: MicroUsdPerToken::from(input / 2),
        estimated_latency_ms: 500,
        tool_reliability: tool,
        reasoning_reliability: tool,
        coding_reliability: code,
        context_reliability: code,
        availability: 100,
        rate_limit_state: RateLimitState::Healthy,
    }
}

fn caps(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn capability_filter_rejects_tool_less_models() {
    let r = Router::new(vec![
        desc("a", "plain", false, 100_000, 4096, econ(2, 8, 80, 80)),
        desc("a", "tooly", true, 100_000, 4096, econ(4, 16, 80, 80)),
    ]);
    let req = RouteRequest {
        required_capabilities: caps(&["tools"]),
        ..Default::default()
    };
    let d = r.route(&req, &[]).unwrap();
    assert_eq!(d.model, "tooly");
}

#[test]
fn empty_capability_set_errors_naming_the_blocker() {
    let r = Router::new(vec![desc(
        "a",
        "m",
        false,
        100_000,
        4096,
        econ(1, 1, 90, 90),
    )]);
    let err = r.route(&RouteRequest::default(), &[]).unwrap_err();
    assert!(err.contains("tools"), "{err}");
}

#[test]
fn cheapest_above_floor_wins() {
    let r = Router::new(vec![
        desc(
            "frontier",
            "big",
            true,
            200_000,
            32_000,
            econ(15, 60, 95, 95),
        ),
        desc("cheap", "fast", true, 100_000, 4096, econ(1, 2, 80, 78)),
    ]);
    let req = RouteRequest {
        quality_floor: 70,
        ..Default::default()
    };
    let d = r.route(&req, &[]).unwrap();
    assert_eq!(d.provider, "cheap", "{}", d.reasoning);
    assert!(d.reasoning.contains("chosen=cheap/fast"));
}

#[test]
fn cheap_below_floor_loses() {
    let r = Router::new(vec![
        desc(
            "frontier",
            "big",
            true,
            200_000,
            32_000,
            econ(15, 60, 95, 95),
        ),
        desc("cheap", "fast", true, 100_000, 4096, econ(1, 2, 40, 40)),
    ]);
    let req = RouteRequest {
        quality_floor: 70,
        ..Default::default()
    };
    let d = r.route(&req, &[]).unwrap();
    assert_eq!(d.model, "big");
}

#[test]
fn local_zero_cost_beats_paid_when_latency_ok() {
    let local = {
        let mut m = econ(0, 0, 82, 82);
        m.estimated_latency_ms = 3000;
        m
    };
    let r = Router::new(vec![
        desc("paid", "fast", true, 100_000, 4096, econ(5, 15, 90, 90)),
        desc("ollama", "qwen3.8", true, 256_000, 8192, local),
    ]);
    let d = r.route(&RouteRequest::default(), &[]).unwrap();
    assert_eq!(d.provider, "ollama");
    assert_eq!(
        d.estimated_cost_micro, 0,
        "local zero-cost models cost zero micro"
    );
}

#[test]
fn tight_latency_preference_excludes_local() {
    let local = {
        let mut m = econ(0, 0, 82, 82);
        m.estimated_latency_ms = 5000;
        m
    };
    let r = Router::new(vec![
        desc("paid", "fast", true, 100_000, 4096, econ(5, 15, 90, 90)),
        desc("ollama", "qwen3.8", true, 256_000, 8192, local),
    ]);
    let req = RouteRequest {
        latency_preference_ms: Some(800),
        ..Default::default()
    };
    let d = r.route(&req, &[]).unwrap();
    assert_eq!(d.provider, "paid");
}

#[test]
fn cache_read_economics_reduces_cost() {
    let e = econ(10, 30, 90, 90);
    let r = Router::new(vec![desc("p", "m", true, 100_000, 4096, e)]);
    let req = RouteRequest {
        context_tokens: 10_000,
        ..Default::default()
    };
    let no_cache = r.route(&req, &[]).unwrap().estimated_cost_micro;
    let cache = CacheState {
        provider: "p".into(),
        model: "m".into(),
        cached_input_tokens: 9_000,
        will_write_tokens: 0,
    };
    let with_cache = r.route(&req, &[cache]).unwrap().estimated_cost_micro;
    assert!(
        with_cache < no_cache,
        "cached call must cost less: {with_cache} vs {no_cache}"
    );
}

#[test]
fn decisions_are_deterministic_and_auditable() {
    let r = Router::new(vec![
        desc("a", "x", true, 100_000, 4096, econ(3, 9, 80, 80)),
        desc("b", "y", true, 100_000, 4096, econ(2, 6, 80, 80)),
        desc("c", "z", true, 100_000, 4096, econ(1, 3, 80, 80)),
    ]);
    let d1 = r.route(&RouteRequest::default(), &[]).unwrap();
    let d2 = r.route(&RouteRequest::default(), &[]).unwrap();
    assert_eq!(d1, d2);
    assert!(d1.reasoning.contains("phase=implement"));
    assert!(d1
        .reasoning
        .contains(&format!("chosen={}/{}", d1.provider, d1.model)));
}

#[test]
fn hard_budget_is_not_overshot() {
    let r = Router::new(vec![
        desc("a", "x", true, 100_000, 4096, econ(100, 300, 90, 90)),
        desc("a", "cheap", true, 100_000, 4096, econ(1, 3, 90, 90)),
    ]);
    let req = RouteRequest {
        task_budget_remaining_micro: 500,
        context_tokens: 100,
        estimated_output_tokens: 10,
        ..Default::default()
    };
    // cheap: 100*1 + 10*3 = 130 micro; big: 100*100 + 10*300 = 13,000.
    let d = r.route(&req, &[]).unwrap();
    assert_eq!(d.model, "cheap");
    assert!(d.estimated_cost_micro <= 500);
}

#[test]
fn micro_rounding_never_understates() {
    let e = econ(1, 1, 80, 80);
    let cost = estimated_call_cost(&e, 1, 1, 0, 0);
    assert!(cost >= 1, "tiny call costs at least 1 micro");
    let big = estimated_call_cost(&e, u64::MAX, u64::MAX, 0, 0);
    assert_eq!(big, u64::MAX, "saturating math never overflows");
}

// ---- RouterService / telemetry / units (audit 9-11) ----

#[test]
fn price_units_equivalence_is_exact() {
    // 1M tokens at $15/Mtok costs $15 = 15_000_000 microUSD, and the
    // formula tokens x price(=15 microUSD/token) yields exactly that.
    let e = econ(15, 60, 90, 90);
    assert_eq!(
        estimated_call_cost(&e, 1_000_000, 0, 0, 0),
        15_000_000,
        "$15/Mtok x 1M tokens = $15 exactly"
    );
    assert_eq!(
        estimated_call_cost(&e, 999_999, 0, 0, 0),
        14_999_985,
        "linear in tokens"
    );
    // No division: tokens x per-token-microUSD is the correct microUSD
    // total; dividing by 1e6 would understate by a factor of a million.
    assert!(
        estimated_call_cost(&e, 1, 0, 0, 0) >= 1,
        "a single token costs at least 1 micro (ceiling)"
    );
}

#[test]
fn service_picks_reliable_over_flaky_cheap() {
    let reliable = {
        let mut e = econ(12, 48, 92, 92);
        e.estimated_latency_ms = 600;
        e
    };
    // Flaky is 3.2x cheaper per call but near-zero success: with
    // once-then-escalate economics, reliability wins only when
    // p_success < cost_flaky/cost_reliable ~ 0.31 at these prices.
    let flaky_e = {
        let mut e = econ(10, 30, 84, 84);
        e.estimated_latency_ms = 300;
        e
    };
    let svc = RouterService::new(vec![
        desc("reliable", "r", true, 512_000, 64_000, reliable),
        desc("flaky", "f", true, 512_000, 64_000, flaky_e),
    ]);
    // The flaky model catastrophically fails 24 of its last 25 phase
    // observations (12.5% record would still win under pure retry
    // economics; near-zero success must not).
    for _ in 0..80 {
        svc.record("flaky", "f", RouterPhase::Implement, false, false, false);
    }
    svc.record("flaky", "f", RouterPhase::Implement, true, false, false);
    let d = svc
        .route(
            &RouteRequest {
                phase: RouterPhase::Implement,
                context_tokens: 40_000,
                estimated_output_tokens: 4_000,
                quality_floor: 70,
                ..Default::default()
            },
            &[],
        )
        .unwrap();
    assert_eq!(
        d.provider, "reliable",
        "expected-cost must favor reliability over raw price: {}",
        d.reasoning
    );
    assert!(d.reasoning.contains("expected-cost"));
}

#[test]
fn static_hard_provider_is_excluded() {
    let mut hard = econ(5, 15, 95, 95);
    hard.rate_limit_state = RateLimitState::Hard;
    let svc = RouterService::new(vec![
        desc("a", "m", true, 512_000, 64_000, econ(5, 15, 95, 95)),
        desc("b", "n", true, 512_000, 64_000, hard),
    ]);
    let d = svc.route(&RouteRequest::default(), &[]).unwrap();
    assert_eq!(d.provider, "a", "Hard provider must be excluded");
}

#[test]
fn runtime_rate_limit_routes_around_after_cooldown() {
    // Both healthy; a live 429 puts the winner into cooldown so the
    // next route picks the other provider (same behavior, different
    // model context preserved where possible).
    let svc = RouterService::new(vec![
        desc("a", "m", true, 512_000, 64_000, econ(5, 15, 95, 95)),
        desc("b", "n", true, 512_000, 64_000, econ(5, 15, 95, 95)),
    ]);
    let d1 = svc.route(&RouteRequest::default(), &[]).unwrap();
    // Both cost the same: deterministic tie-break -> 'a'.
    assert_eq!(d1.provider, "a");
    svc.record("a", "m", RouterPhase::Implement, false, false, true);
    assert!(svc.telemetry.cooldown_active("a"));
    let d2 = svc.route(&RouteRequest::default(), &[]).unwrap();
    assert_eq!(d2.provider, "b", "cooldown routes around the limiter");
    assert!(d2.reasoning.contains("expected-cost"));
}

// ---- prefix-cache stability integration (audits 65-66) ----

fn prefix_turn(id: u64, bytes: &[u8]) -> stability::TurnPrefix {
    // FNV-1a-derived deterministic content digest (router has no hash
    // dep): identical bytes must hash identically regardless of turn id.
    let mut h = [0u8; 32];
    let mut acc = 0xcbf29ce484222325u64;
    for &b in bytes {
        acc ^= u64::from(b);
        acc = acc.wrapping_mul(0x100000001b3);
    }
    h[..8].copy_from_slice(&acc.to_le_bytes());
    h[8..16].copy_from_slice(&acc.wrapping_mul(31).to_le_bytes());
    h[16..24].copy_from_slice(&acc.wrapping_mul(97).to_le_bytes());
    h[24..].copy_from_slice(&acc.wrapping_mul(211).to_le_bytes());
    stability::TurnPrefix::new(id, h, bytes.len() as u32)
}

#[test]
fn churn_premium_scales_estimate_and_stays_auditable_and_deterministic() {
    let svc = RouterService::new(vec![desc(
        "p",
        "m",
        true,
        100_000,
        4096,
        econ(1, 1, 90, 90),
    )]);
    let req = RouteRequest {
        context_tokens: 100,
        estimated_output_tokens: 10,
        ..Default::default()
    };
    let base = svc.route(&req, &[]).unwrap();
    assert_eq!(base.estimated_cost_micro, 110);
    // Stable history (identical prefixes): no premium, decision equal.
    let stable: Vec<u8> = vec![b's'; 40];
    let healthy = [
        prefix_turn(1, &stable),
        prefix_turn(2, &stable),
        prefix_turn(3, &stable),
    ];
    let d = svc
        .route_with_prefix_stability(&req, &[], 0.8, Some(&healthy))
        .unwrap();
    assert_eq!(d, base);
    // Churning history: same-length rewritten prefix -> stability 0.0 ->
    // penalty 0.25 -> estimate *= 1.25, rounded up (110 + ceil(27.5) = 138).
    let rewritten: Vec<u8> = vec![b'r'; 40];
    assert_eq!(stable.len(), rewritten.len());
    assert_ne!(
        prefix_turn(2, &rewritten).prefix_hash,
        prefix_turn(1, &stable).prefix_hash
    );
    let churny = [prefix_turn(1, &stable), prefix_turn(2, &rewritten)];
    let d = svc
        .route_with_prefix_stability(&req, &[], 0.8, Some(&churny))
        .unwrap();
    assert_eq!(d.estimated_cost_micro, 138);
    assert!(
        d.reasoning.contains("prefix_stability=0.000"),
        "{}",
        d.reasoning
    );
    assert!(
        d.reasoning.contains("churn_penalty=0.2500"),
        "{}",
        d.reasoning
    );
    // Deterministic: identical history reproduces the decision exactly.
    let d2 = svc
        .route_with_prefix_stability(&req, &[], 0.8, Some(&churny))
        .unwrap();
    assert_eq!(d, d2);
    // No history / empty history: identical to the plain route.
    assert_eq!(
        svc.route_with_prefix_stability(&req, &[], 0.8, None)
            .unwrap(),
        base
    );
    assert_eq!(
        svc.route_with_prefix_stability(&req, &[], 0.8, Some(&[]))
            .unwrap(),
        base
    );
    // A floor of 0 disables the premium (nothing is below it).
    let d = svc
        .route_with_prefix_stability(&req, &[], 0.0, Some(&churny))
        .unwrap();
    assert_eq!(d, base);
    // Mid-churn stability scales proportionally and stays in (0, 0.25].
    let mut grown = stable.clone();
    grown.extend_from_slice(&vec![b'z'; 300]);
    let partial = [prefix_turn(1, &stable), prefix_turn(2, &stable), {
        let mut tp = prefix_turn(3, &grown);
        tp.prefix_tokens = 300; // growth ratio 40/300 -> very low
        tp
    }];
    let d = svc
        .route_with_prefix_stability(&req, &[], 0.8, Some(&partial))
        .unwrap();
    assert!(d.estimated_cost_micro > base.estimated_cost_micro);
    assert!(d.estimated_cost_micro < 138);
    assert!(d.reasoning.contains("churn_penalty=0."));
}

#[test]
fn churn_premium_never_leaks_into_plain_routing() {
    // The plain surface is byte-stable under the new machinery.
    let svc = RouterService::new(vec![desc(
        "p",
        "m",
        true,
        100_000,
        4096,
        econ(1, 1, 90, 90),
    )]);
    let a = svc.route(&RouteRequest::default(), &[]).unwrap();
    let b = svc.route(&RouteRequest::default(), &[]).unwrap();
    assert_eq!(a, b);
    assert!(!a.reasoning.contains("churn_penalty"));
}

#[test]
fn churn_zeroes_cache_read_discounts_and_can_flip_the_choice() {
    // P0-82 adversarial: a candidate whose cheapness comes ONLY from
    // provider-side cache reads wins a stable session and LOSES a
    // churning one (stability 0.3 < floor 0.8) — the route is priced
    // as if its cache misses and the choice flips to the flat-price
    // candidate, never just a scaled price tag.
    let mut x_econ = econ(10, 1, 90, 90);
    x_econ.cache_read_price_per_mtok = MicroUsdPerToken::from(1);
    x_econ.cache_write_price_per_mtok = MicroUsdPerToken::from(1);
    x_econ.estimated_latency_ms = 200;
    let mut y_econ = econ(5, 1, 90, 90);
    y_econ.estimated_latency_ms = 300;
    let svc = RouterService::new(vec![
        desc("cache", "cx", true, 100_000, 4096, x_econ),
        desc("flat", "fy", true, 100_000, 4096, y_econ),
    ]);
    let req = RouteRequest {
        context_tokens: 1000,
        estimated_output_tokens: 10,
        ..Default::default()
    };
    // The session's own last turn wrote a cache; with a STABLE prefix
    // 900 of the 1000 input tokens hit it.
    let cache = [CacheState {
        provider: "cache".into(),
        model: "cx".into(),
        cached_input_tokens: 900,
        will_write_tokens: 1000,
    }];
    let stable = [
        prefix_turn(1, b"stable-prefix-bytes"),
        prefix_turn(2, b"stable-prefix-bytes"),
    ];
    let d = svc
        .route_with_prefix_stability(&req, &cache, 0.8, Some(&stable))
        .unwrap();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        ("cache", "cx"),
        "cache-read discount wins a stable session: {}",
        d.reasoning
    );
    // Churning history: the last pair REWRITES to a longer, different
    // prefix (40 -> 130 tokens): growth scores 40/130 ~ 0.154 (below
    // the 0.8 floor) — the churn premium applies.
    let churny = [
        prefix_turn(1, b"stable-prefix-bytes"),
        prefix_turn(2, b"stable-prefix-bytes"),
        {
            let mut tp = prefix_turn(
                3,
                b"stable-prefix-bytes-grown-longer-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            );
            tp.prefix_tokens = 130;
            tp
        },
    ];
    let d2 = svc
        .route_with_prefix_stability(&req, &cache, 0.8, Some(&churny))
        .unwrap();
    assert_eq!(
        (d2.provider.as_str(), d2.model.as_str()),
        ("flat", "fy"),
        "a churning session must be priced without its cache reads: {}",
        d2.reasoning
    );
    assert!(
        d2.reasoning.contains("prefix_stability=0.146"),
        "{}",
        d2.reasoning
    );
    assert!(d2.reasoning.contains("churn_penalty="), "{}", d2.reasoning);
    // The chosen decision still carries the churn premium on its cost
    // (flat base 5010 * (1 + penalty) with penalty ~0.204).
    let flat_base = 1000 * 5 + 10;
    assert!(
        d2.estimated_cost_micro > flat_base,
        "premium must inflate the estimate: {} > {flat_base}",
        d2.estimated_cost_micro
    );
    // Determinism.
    let d3 = svc
        .route_with_prefix_stability(&req, &cache, 0.8, Some(&churny))
        .unwrap();
    assert_eq!(d2, d3);
}

#[test]
fn segment_observations_replace_the_binary_rule_and_legacy_rows_stay_identical() {
    // Audits 45/82 end at the routing consult: when the durable rows
    // carry segment observations, cache economics consume the MEASURED
    // longest stable prefix — a same-length volatile-tail rewrite that
    // the binary digest pair would score 0.0 scores its true coverage
    // (1.0), and a rewrite inside the cacheable prefix scores exactly
    // the surviving share. Legacy rows keep the binary verdict.
    let svc = RouterService::new(vec![desc(
        "p",
        "m",
        true,
        100_000,
        4096,
        econ(1, 1, 90, 90),
    )]);
    let req = RouteRequest {
        context_tokens: 100,
        estimated_output_tokens: 10,
        ..Default::default()
    };
    let base = svc.route(&req, &[]).unwrap();
    assert_eq!(base.estimated_cost_micro, 110);

    let tokens = [10u64, 20, 30, 40, 50, 5, 4, 3];
    let ids: Vec<u8> = (0..8).collect();
    let json = |ids: &[u8]| {
        let hashes: Vec<String> = ids.iter().map(|&i| format!("{i:02x}").repeat(32)).collect();
        serde_json::json!({
            "segment_hashes": hashes,
            "segment_token_counts": tokens,
            "cache_read_tokens": 0u64,
        })
        .to_string()
    };
    let history = |second_ids: &[u8]| {
        stability::TurnPrefix::history_from_persisted(vec![
            (1, [1u8; 32], 150, Some(json(&ids))),
            (2, [2u8; 32], 150, Some(json(second_ids))),
        ])
    };

    // Volatile-only change (first volatile segment): fully covered. The
    // binary hashes DIFFER with equal token counts — the old rule would
    // score 0.0 and charge the premium; the measured rule scores 1.0.
    let mut volatile = ids.clone();
    volatile[5] = 90;
    let h = history(&volatile);
    assert_eq!(h[1].stable_leading_tokens, Some(150));
    let d = svc
        .route_with_prefix_stability(&req, &[], 0.8, Some(&h))
        .unwrap();
    assert_eq!(d, base, "measured full coverage must not charge churn");
    assert!(!d.reasoning.contains("churn_penalty"));

    // Rewrite inside the cacheable prefix (segment 2): 10+20 of the
    // 150 prefix tokens survive -> stability 0.2 -> premium applies.
    let mut mid = ids.clone();
    mid[2] = 91;
    let h = history(&mid);
    assert_eq!(h[1].stable_leading_tokens, Some(30));
    let d = svc
        .route_with_prefix_stability(&req, &[], 0.8, Some(&h))
        .unwrap();
    assert!(
        d.reasoning.contains("prefix_stability=0.200"),
        "{}",
        d.reasoning
    );
    assert!(d.reasoning.contains("churn_penalty="), "{}", d.reasoning);
    assert_eq!(d.estimated_cost_micro, 131); // ceil(110 * 1.1875)

    // Legacy fallback: no segment payloads anywhere -> byte-identical
    // to the pre-v19 binary history (same-length rewrite -> 0.0 -> 138).
    let legacy = vec![prefix_turn(1, b"same-length-old"), {
        let mut tp = prefix_turn(2, b"same-length-new");
        tp.prefix_tokens = prefix_turn(1, b"same-length-old").prefix_tokens;
        tp
    }];
    let legacy_decision = svc
        .route_with_prefix_stability(&req, &[], 0.8, Some(&legacy))
        .unwrap();
    assert!(legacy_decision.reasoning.contains("prefix_stability=0.000"));
    assert_eq!(legacy_decision.estimated_cost_micro, 138);
    let no_payloads = stability::TurnPrefix::history_from_persisted(vec![
        (1, legacy[0].prefix_hash, legacy[0].prefix_tokens, None),
        (2, legacy[1].prefix_hash, legacy[1].prefix_tokens, None),
    ]);
    assert_eq!(
        svc.route_with_prefix_stability(&req, &[], 0.8, Some(&no_payloads))
            .unwrap(),
        legacy_decision,
        "rows without the v19 column route byte-identically"
    );
}

#[test]
fn poisoned_telemetry_caches_are_recovered_not_propagated() {
    let svc = RouterService::new(vec![
        desc("a", "am", true, 100_000, 4096, econ(5, 15, 95, 95)),
        desc("b", "bm", true, 100_000, 4096, econ(5, 15, 95, 95)),
    ]);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = svc.telemetry.inner.lock().unwrap();
        panic!("holder poisoned the outcome cache");
    }));
    assert!(svc.telemetry.inner.is_poisoned());
    // Cache => recover: advisory EWMA stats keep serving and rate-limit
    // cooldowns keep recording instead of panicking the routing path.
    svc.telemetry
        .record_outcome("a", "am", RouterPhase::Implement, true, false, false, 12);
    svc.telemetry.record_rate_limit("b", 30);
    assert!(svc.telemetry.cooldown_active("b"));
    let health = svc.telemetry.snapshot();
    assert!(health.success_ppm("a", "am", RouterPhase::Implement) > 0);
    assert!(!svc.telemetry.inner.is_poisoned());
}

#[test]
fn telemetry_outcome_records_latency_priors_and_isolated_reliability() {
    // The outcome record entry: settled calls feed success + latency
    // per (provider, model, phase); reliability priors move ONLY for
    // the failing instance pair and rate-limit cooldown touches only
    // the limited provider.
    let svc = RouterService::new(vec![
        desc("a", "am", true, 100_000, 4096, econ(5, 15, 95, 95)),
        desc("b", "bm", true, 100_000, 4096, econ(5, 15, 95, 95)),
    ]);
    let before = svc.telemetry.snapshot();
    let a_before = before.success_ppm("a", "am", RouterPhase::Implement);
    // N settled calls against (a, am): 9 failures + 1 success with
    // latency; (b, bm) untouched.
    for _ in 0..9 {
        svc.record_outcome("a", "am", RouterPhase::Implement, false, false, false, 400);
    }
    svc.record_outcome("a", "am", RouterPhase::Implement, true, false, false, 200);
    let after = svc.telemetry.snapshot();
    let a_after = after.success_ppm("a", "am", RouterPhase::Implement);
    assert!(
        a_after < a_before,
        "(a, am) reliability must fall: {a_before} -> {a_after}"
    );
    // Every OTHER pair keeps its prior exactly (the map only gains the
    // observed key; unobserved keys still resolve to DEFAULT_SUCCESS_PPM).
    let b_after = after.success_ppm("b", "bm", RouterPhase::Implement);
    let b_before = before.success_ppm("b", "bm", RouterPhase::Implement);
    assert_eq!(a_before, 800_000);
    assert_eq!(b_before, 800_000);
    assert_eq!(b_after, 800_000, "unobserved pair untouched");
    let review_after = after.success_ppm("a", "am", RouterPhase::Review);
    assert_eq!(review_after, 800_000, "other phases untouched");
    // Latency EWMA tracked the recorded outcome.
    let avg = svc
        .telemetry
        .avg_latency_ms("a", "am", RouterPhase::Implement);
    assert!(avg > 200.0 && avg < 400.0, "latency ewma: {avg}");
    // Rate limit: cooldown ONLY for the limited provider.
    svc.record_outcome("a", "am", RouterPhase::Implement, false, false, true, 500);
    assert!(svc.telemetry.cooldown_active("a"));
    assert!(!svc.telemetry.cooldown_active("b"));
}

// ==================================================================
// Single-authoritative-qualification (audit P0-3/P0-89), typed-price
// units (P0-5) and the explicit scoring ladder (P0-4). Adversarial:
// every axis tries to smuggle an unqualified ultra-cheap model into a
// decision, and every path must refuse it.
// ==================================================================

fn tiny_req(names: &[&str], floor: u8) -> RouteRequest {
    RouteRequest {
        required_capabilities: caps(names),
        context_tokens: 2,
        estimated_output_tokens: 1,
        quality_floor: floor,
        ..Default::default()
    }
}

fn good_desc() -> ModelDescriptor {
    desc("good", "gm", true, 100_000, 4096, econ(300, 1, 90, 90))
}

fn cheap_bad_desc() -> ModelDescriptor {
    // 2 tokens x 1 microUSD/token: a ~2-microUSD competitor.
    desc("bad", "bm", true, 100_000, 4096, econ(1, 1, 90, 90))
}

/// Both selection paths must choose `good`; `bad` (whatever axis it
/// violates) must never be chosen — the single qualification pass is
/// the ONLY filter, so cost can never re-admit a filtered candidate.
fn assert_paths_never_select_bad(
    good: &ModelDescriptor,
    bad: &ModelDescriptor,
    req: &RouteRequest,
    label: &str,
) {
    let plain = Router::new(vec![good.clone(), bad.clone()]);
    let d = plain
        .route(req, &[])
        .unwrap_or_else(|e| panic!("{label}: plain route failed: {e}"));
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        (good.provider.as_str(), good.model.as_str()),
        "{label}: plain route selected an unqualified candidate: {}",
        d.reasoning
    );
    let svc = RouterService::new(vec![good.clone(), bad.clone()]);
    let d = svc
        .route(req, &[])
        .unwrap_or_else(|e| panic!("{label}: service route failed: {e}"));
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        (good.provider.as_str(), good.model.as_str()),
        "{label}: service route selected an unqualified candidate: {}",
        d.reasoning
    );
    assert!(
        d.reasoning
            .contains(&format!("chosen={}/{}", good.provider, good.model)),
        "{label}: reasoning must name the qualified winner: {}",
        d.reasoning
    );
}

#[test]
fn unqualified_ultra_cheap_candidates_never_win_any_axis() {
    let tools_req = tiny_req(&["tools", "streaming"], 60);
    // 1. Missing the tools capability (the canonical P0-3 shape: the
    //    cheapest eligible model cannot serve the request at all).
    let bad = {
        let mut d = cheap_bad_desc();
        d.tools = false;
        d
    };
    assert_paths_never_select_bad(&good_desc(), &bad, &tools_req, "tools axis");
    // 2. Missing parallel tools.
    let bad = {
        let mut d = cheap_bad_desc();
        d.parallel_tools = false;
        d
    };
    assert_paths_never_select_bad(
        &good_desc(),
        &bad,
        &tiny_req(&["parallel_tools"], 60),
        "parallel_tools axis",
    );
    // 3. Context window too small for the request.
    let bad = {
        let mut d = cheap_bad_desc();
        d.context = 1;
        d
    };
    assert_paths_never_select_bad(&good_desc(), &bad, &tools_req, "context axis");
    // 4. Max output too small.
    let bad = {
        let mut d = cheap_bad_desc();
        d.max_output = 0;
        d
    };
    assert_paths_never_select_bad(&good_desc(), &bad, &tools_req, "output axis");
    // 5. Below the quality floor (cheap garbage must lose to floor).
    let good = desc("good", "gm", true, 100_000, 4096, econ(300, 1, 85, 85));
    let bad = {
        let mut d = cheap_bad_desc();
        d.economics = econ(1, 1, 40, 40);
        d
    };
    assert_paths_never_select_bad(
        &good,
        &bad,
        &tiny_req(&["tools", "streaming"], 80),
        "quality-floor axis",
    );
    // 6. Exceeds the latency preference.
    let bad = {
        let mut d = cheap_bad_desc();
        d.economics.estimated_latency_ms = 5000;
        d
    };
    let req = RouteRequest {
        latency_preference_ms: Some(700),
        ..tiny_req(&["tools", "streaming"], 60)
    };
    assert_paths_never_select_bad(&good_desc(), &bad, &req, "latency axis");
    // 7. Over the hard request budget (only the expensive one may fit).
    let good = desc("good", "gm", true, 100_000, 4096, econ(1, 1, 90, 90));
    let bad = desc("bad", "bm", true, 100_000, 4096, econ(300, 1, 90, 90));
    let req = RouteRequest {
        task_budget_remaining_micro: 3,
        ..tiny_req(&["tools", "streaming"], 60)
    };
    assert_paths_never_select_bad(&good, &bad, &req, "budget axis");
    // 8. Static Hard rate-limit state — excluded by BOTH paths (the
    //    plain router now consumes the same qualification pass).
    let bad = {
        let mut d = cheap_bad_desc();
        d.economics.rate_limit_state = RateLimitState::Hard;
        d
    };
    assert_paths_never_select_bad(&good_desc(), &bad, &tools_req, "hard-rate-limit axis");
}

/// P1 item 10: when a turn carries image parts the runtime adds the
/// `vision` capability requirement; qualification must then exclude
/// every non-vision candidate (fail closed) while the same candidates
/// stay eligible for a text-only request.
#[test]
fn vision_requirement_excludes_non_vision_candidates() {
    let vision = {
        let mut d = good_desc();
        d.vision = true;
        d
    };
    let no_vision = cheap_bad_desc();
    assert!(!no_vision.vision, "the cheap competitor is vision-less");
    let req = tiny_req(&["tools", "streaming", "vision"], 60);
    assert_paths_never_select_bad(&vision, &no_vision, &req, "vision axis");
    // With no vision-capable candidate the route is a typed capability
    // refusal NAMING the missing capability (never a silent downgrade).
    let only_no_vision = Router::new(vec![no_vision.clone()]);
    let err = only_no_vision.route(&req, &[]).unwrap_err();
    assert!(
        err.to_string().contains("vision"),
        "the refusal must name the missing capability: {err}"
    );
    // Without the requirement the same non-vision model is eligible:
    // the filter is caused by the request, not by hidden defaults.
    let text_req = tiny_req(&["tools", "streaming"], 60);
    let d = Router::new(vec![no_vision]).route(&text_req, &[]).unwrap();
    assert_eq!(d.model, "bm");
}

#[test]
fn soft_rate_limit_is_not_a_qualification_blocker() {
    // Only Hard state and an ACTIVE cooldown exclude; Soft is an
    // advisory signal and must not disable a cheap qualified model.
    let mut soft = cheap_bad_desc();
    soft.economics.rate_limit_state = RateLimitState::Soft;
    let good = desc("good", "gm", true, 100_000, 4096, econ(300, 1, 90, 90));
    let req = tiny_req(&["tools", "streaming"], 60);
    let d = RouterService::new(vec![good, soft.clone()])
        .route(&req, &[])
        .unwrap();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        (soft.provider.as_str(), soft.model.as_str()),
        "Soft rate-limit state is not a Hard exclusion: {}",
        d.reasoning
    );
}

#[test]
fn live_cooldown_excludes_only_via_the_service_health_view() {
    // P0-89: live cooldown state is REAL state fed into the single
    // qualification pass, not the old inert always-false helper.
    let svc = RouterService::new(vec![good_desc(), cheap_bad_desc()]);
    let req = tiny_req(&["tools", "streaming"], 60);
    // The cheap bad provider takes a 429: it enters the telemetry
    // cooldown window and the NEXT service decision must route around.
    svc.record("bad", "bm", RouterPhase::Implement, false, false, true);
    assert!(svc.telemetry.cooldown_active("bad"));
    let d = svc.route(&req, &[]).unwrap();
    assert_eq!(
        d.provider, "good",
        "cooldown must route around bad: {}",
        d.reasoning
    );
    // The plain path has no live view by design: it legitimately still
    // sees bad (static axes only). This documents the boundary.
    let d = Router::new(vec![good_desc(), cheap_bad_desc()])
        .route(&req, &[])
        .unwrap();
    assert_eq!(d.provider, "bad", "plain routing carries no live cooldown");
}

#[test]
fn escalation_pool_contains_only_qualified_candidates_after_two_strikes() {
    // A = cheap qualified workhorse that is failing; B = ultra-cheap
    // (1 microUSD/token) but WITHOUT tools — the old service loop would
    // happily escalate against B's price and could select it. C = the
    // expensive qualified frontier model.
    let flaky = {
        let mut e = econ(50, 1, 90, 90);
        e.estimated_latency_ms = 500;
        e
    };
    let mut b_no_tools = desc("bad", "bm", true, 100_000, 4096, econ(1, 1, 99, 99));
    b_no_tools.tools = false;
    let premium = {
        let mut e = econ(400, 1, 95, 95);
        e.estimated_latency_ms = 700;
        e
    };
    let candidates = vec![
        desc("a", "am", true, 100_000, 4096, flaky),
        b_no_tools,
        desc("c", "cm", true, 100_000, 4096, premium),
    ];
    let svc = RouterService::new(candidates.clone());
    let req = RouteRequest {
        // 1 input token, 0 output: per-token prices are the base costs.
        context_tokens: 1,
        estimated_output_tokens: 0,
        ..tiny_req(&["tools", "streaming"], 60)
    };
    // Two strikes plus: the cheap workhorse keeps failing.
    for _ in 0..30 {
        svc.record("a", "am", RouterPhase::Implement, false, false, false);
    }
    // The single qualification pass admits exactly the tooled pair.
    let health = svc.telemetry.snapshot();
    let qualified = qualified_candidates(&candidates, &req, &[], &health).unwrap();
    assert_eq!(qualified.len(), 2, "only a and c qualify: {qualified:?}");
    assert!(
        qualified.iter().all(|q| q.descriptor.provider != "bad"),
        "the unqualified cheap model must not appear in the qualified vector"
    );
    // Escalation math must reference C (400 micro), never B (1 micro):
    // two_cheapest_distinct over the QUALIFIED set.
    let two = two_cheapest_distinct(&qualified);
    assert_eq!(
        two.cheapest.map(|(c, p, _)| (c, p.to_string())),
        Some((50, "a".into()))
    );
    assert_eq!(
        two.second.map(|(c, p, _)| (c, p.to_string())),
        Some((400, "c".into()))
    );
    let a_scored = score_candidates(&qualified, RouterPhase::Implement, &EmptyOutcomeStore)
        .into_iter()
        .find(|s| s.candidate.provider == "a")
        .expect("a is qualified");
    let esc_c = 400u128;
    assert_eq!(
        a_scored.expected_cost_micro,
        expected_cost_to_success(a_scored.success_ppm, 50, esc_c),
        "expected cost must escalate to the qualified C, never to B"
    );
    // Routing under the strikes: A stays the expected-cost winner and
    // B is never chosen, on every re-route (service and plain).
    for _ in 0..3 {
        let d = svc.route(&req, &[]).unwrap();
        assert_ne!(
            d.provider, "bad",
            "never escalate INTO the unqualified model"
        );
        assert!(d.provider == "a" || d.provider == "c");
    }
    let d = Router::new(candidates).route(&req, &[]).unwrap();
    assert_eq!(d.provider, "a");
}

#[test]
fn maximum_quality_path_consumes_only_qualified_candidates() {
    // A 95 floor: only the frontier model qualifies; the 99-quality
    // tool-less ultra-cheap model and the 90-quality workhorse must
    // both lose on BOTH paths.
    let mut b_no_tools = desc("bad", "bm", true, 100_000, 4096, econ(1, 1, 99, 99));
    b_no_tools.tools = false;
    let candidates = vec![
        desc("a", "am", true, 100_000, 4096, econ(50, 1, 90, 90)),
        b_no_tools,
        desc("c", "cm", true, 100_000, 4096, econ(400, 1, 95, 95)),
    ];
    let req = tiny_req(&["tools", "streaming"], 95);
    let d = Router::new(candidates.clone()).route(&req, &[]).unwrap();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        ("c", "cm"),
        "{}",
        d.reasoning
    );
    let d = RouterService::new(candidates).route(&req, &[]).unwrap();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        ("c", "cm"),
        "{}",
        d.reasoning
    );
}

#[test]
fn qualified_candidates_stages_its_denials() {
    // Direct API: denials carry staged counts so each path reproduces
    // its own denial message; qualification is a single pass.
    let tools_req = tiny_req(&["tools", "streaming"], 60);
    let good = good_desc();
    let mut no_tools = cheap_bad_desc();
    no_tools.tools = false;
    let mut too_small_ctx = cheap_bad_desc();
    too_small_ctx.context = 1;
    let mut low_quality = cheap_bad_desc();
    low_quality.economics = econ(1, 1, 40, 40);
    let mut hard = cheap_bad_desc();
    hard.economics.rate_limit_state = RateLimitState::Hard;
    // Stage 1: capability/fit empties the set -> the capability error,
    // naming exactly the capability NO health-passing candidate has.
    let f = qualified_candidates(&[no_tools.clone()], &tools_req, &[], &LiveHealth::default())
        .unwrap_err();
    assert_eq!(f.fit_survivors, 0, "no candidate clears capability/fit");
    assert_eq!(f.quality_survivors, 0);
    assert_eq!(f.unsupported_capabilities, vec!["tools".to_string()]);
    assert!(f.route_error().contains("missing: tools"));
    // Stage 2: fit passes but the floor empties the set -> quality error.
    let f = qualified_candidates(
        &[too_small_ctx, low_quality.clone()],
        &tiny_req(&["tools", "streaming"], 80),
        &[],
        &LiveHealth::default(),
    )
    .unwrap_err();
    assert_eq!(f.fit_survivors, 1);
    assert_eq!(f.quality_survivors, 0);
    assert!(f.route_error().contains("quality floor 80"));
    // Stage 3: fit and quality pass but budget/latency empties the set
    // -> budget error, with both earlier counts still positive.
    let over_budget_req = RouteRequest {
        task_budget_remaining_micro: 1,
        ..tools_req.clone()
    };
    let f =
        qualified_candidates(&[good], &over_budget_req, &[], &LiveHealth::default()).unwrap_err();
    assert_eq!(f.fit_survivors, 1);
    assert_eq!(f.quality_survivors, 1);
    assert!(f.route_error().contains("budget"));
    // Health empties the set BEFORE capability counting: a Hard model
    // does not "support" a capability it could never serve on.
    let f = qualified_candidates(&[hard], &tools_req, &[], &LiveHealth::default()).unwrap_err();
    assert_eq!(f.fit_survivors, 0);
    assert_eq!(
        f.unsupported_capabilities,
        vec!["tools".to_string(), "streaming".to_string()]
    );
}

#[test]
fn plain_denials_name_the_same_stages_as_the_service() {
    let tools_req = tiny_req(&["tools", "streaming"], 60);
    let mut no_tools = cheap_bad_desc();
    no_tools.tools = false;
    let r = Router::new(vec![no_tools.clone()]);
    let err = r.route(&tools_req, &[]).unwrap_err();
    assert!(err.contains("missing: tools"), "{err}");
    let mut low = cheap_bad_desc();
    low.economics = econ(1, 1, 40, 40);
    let r = Router::new(vec![low]);
    let err = r
        .route(&tiny_req(&["tools", "streaming"], 80), &[])
        .unwrap_err();
    assert!(err.contains("quality floor 80"), "{err}");
    let r = Router::new(vec![good_desc()]);
    let err = r
        .route(
            &RouteRequest {
                task_budget_remaining_micro: 1,
                ..tools_req.clone()
            },
            &[],
        )
        .unwrap_err();
    assert!(err.contains("budget"), "denial must name the budget: {err}");
    // The service maps the SAME staged failure to the same strings.
    let svc = RouterService::new(vec![no_tools]);
    let err = svc.route(&tools_req, &[]).unwrap_err();
    assert!(err.contains("missing: tools"), "{err}");
}

// ---- P0-4: the explicit scoring ladder, with documented units ----

fn scored<'a>(
    d: &'a ModelDescriptor,
    expected_cost_micro: u64,
    expected_latency_ms: u64,
    success_ppm: u32,
    call_cost_micro: u64,
) -> ScoredCandidate<'a> {
    ScoredCandidate {
        candidate: d,
        expected_cost_micro,
        expected_latency_ms,
        success_ppm,
        call_cost_micro,
        work_estimate: None,
    }
}

#[test]
fn scoring_ladder_is_explicit_and_unit_honest() {
    let a = desc("a", "x", true, 100_000, 4096, econ(300, 1, 90, 90));
    let b = desc("b", "y", true, 100_000, 4096, econ(300, 1, 90, 90));
    // (1) Expected cost-to-success asc dominates everything.
    let sa = scored(&a, 100, 500, 800_000, 300);
    let sb = scored(&b, 101, 1, 900_000, 1);
    assert_eq!(sa.compare(&sb), std::cmp::Ordering::Less);
    // (2) Equal expected cost: higher success ppm wins.
    let sa = scored(&a, 100, 500, 900_000, 300);
    let sb = scored(&b, 100, 500, 500_000, 300);
    assert_eq!(sa.compare(&sb), std::cmp::Ordering::Less);
    // (3) Equal expected + ppm: lower latency (ms) wins.
    let sa = scored(&a, 100, 250, 800_000, 300);
    let sb = scored(&b, 100, 900, 800_000, 300);
    assert_eq!(sa.compare(&sb), std::cmp::Ordering::Less);
    // (4) Equal expected + ppm + latency: lower call cost (microUSD) wins.
    let sa = scored(&a, 100, 500, 800_000, 200);
    let sb = scored(&b, 100, 500, 800_000, 40);
    assert_eq!(sa.compare(&sb), std::cmp::Ordering::Greater);
    // (5) Fully equal scores: (provider, model) lex asc is the
    // deterministic final key.
    let sa = scored(&a, 100, 500, 800_000, 300);
    let sb = scored(&b, 100, 500, 800_000, 300);
    assert_eq!(sa.compare(&sb), std::cmp::Ordering::Less, "a/x < b/y");
    assert_eq!(sb.compare(&sa), std::cmp::Ordering::Greater);
    let sa2 = scored(&a, 100, 500, 800_000, 300);
    assert_eq!(sa.compare(&sa2), std::cmp::Ordering::Equal);
}

#[test]
fn regression_old_tie_break_compared_cost_to_latency() {
    // Old RouterService tie-break: expected costs equal -> better iff
    // `base < previous_candidate.estimated_latency_ms` — comparing a
    // microUSD cost against MILLISECONDS. A and B share a 40-micro base
    // (identical economics, latency differs), hence exactly equal
    // expected costs; the old rule then judged B "better" because
    // `base_B (40 micro) < latency_A (100 ms)`. The explicit ladder
    // must descend deterministically to latency: A (100 ms) over B.
    let mut e_a = econ(40, 1, 90, 90);
    e_a.estimated_latency_ms = 100;
    let mut e_b = econ(40, 1, 90, 90);
    e_b.estimated_latency_ms = 1000;
    let candidates = vec![
        desc("a", "am", true, 100_000, 4096, e_a),
        desc("b", "bm", true, 100_000, 4096, e_b),
    ];
    let req = RouteRequest {
        context_tokens: 1,
        estimated_output_tokens: 0,
        ..tiny_req(&["tools", "streaming"], 60)
    };
    let d = RouterService::new(candidates).route(&req, &[]).unwrap();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        ("a", "am"),
        "the ladder picks lower latency on equal expected cost: {}",
        d.reasoning
    );
    // Equal-everything candidates resolve by (provider, model) key.
    let candidates = vec![
        desc("z", "zm", true, 100_000, 4096, econ(300, 1, 90, 90)),
        desc("a", "am", true, 100_000, 4096, econ(300, 1, 90, 90)),
    ];
    let d = RouterService::new(candidates).route(&req, &[]).unwrap();
    assert_eq!(
        d.provider, "a",
        "deterministic key tie-break: {}",
        d.reasoning
    );
    // Equal-cost, differing-latency: repeated routes stay deterministic.
    let repeated = RouterService::new(vec![
        desc("a", "am", true, 100_000, 4096, {
            let mut e = econ(40, 1, 90, 90);
            e.estimated_latency_ms = 100;
            e
        }),
        desc("b", "bm", true, 100_000, 4096, {
            let mut e = econ(40, 1, 90, 90);
            e.estimated_latency_ms = 1000;
            e
        }),
    ]);
    for _ in 0..3 {
        let d = repeated
            .route(
                &RouteRequest {
                    context_tokens: 1,
                    estimated_output_tokens: 0,
                    ..tiny_req(&["tools", "streaming"], 60)
                },
                &[],
            )
            .unwrap();
        assert_eq!(d.provider, "a", "repeated routes stay deterministic");
    }
}

#[test]
fn expected_cost_is_integer_exact_and_saturating() {
    // P(fail)/2 terms round UP (never understate), sums saturate.
    let e = expected_cost_to_success(800_000, 1_000, 3_000u128);
    // base 1000 + ceil(1000*0.2/2)=100 + ceil(3000*0.1)=300.
    assert_eq!(e, 1_400);
    assert_eq!(expected_cost_to_success(1_000_000, 500, 1u128), 500);
    assert_eq!(
        expected_cost_to_success(0, u64::MAX, u128::MAX),
        u64::MAX,
        "hostile magnitudes saturate, never panic"
    );
    assert_eq!(
        expected_cost_to_success(500_000, 7, 7u128),
        // ceil(7*0.25)=2 and ceil(7*0.25)=2 -> 11
        11
    );
}

// ---- P0-5: typed price units flow through routing untouched ----

#[test]
fn wrapper_arithmetic_equals_the_legacy_interpretation() {
    // The typed field keeps the exact legacy numbers: tokens x price
    // with no division, no floats, no rounding mid-route.
    let e = econ(15, 60, 90, 90);
    assert_eq!(e.input_price_per_mtok.0, 15);
    assert_eq!(e.output_price_per_mtok.0, 60);
    assert_eq!(
        estimated_call_cost(&e, 1_000_000, 0, 0, 0),
        15_000_000,
        "$15/Mtok x 1M tokens = $15 exactly (typed fields)"
    );
    // The conversion constructor used by catalog ingestion documents
    // the dollars-per-million reading at the boundary.
    let from_usd = MicroUsdPerToken::from_dollars_per_million(15);
    assert_eq!(from_usd.saturating_mul(1_000_000), 15_000_000);
}

#[test]
fn budget_denial_never_confuses_units() {
    // A latency value can no longer BE a price: the fields reject the
    // mix at compile time (see the core compile_fail doc example), and
    // at runtime the budget axis only ever compares microUSD to
    // microUSD.
    let mut e = econ(100, 100, 90, 90);
    e.estimated_latency_ms = 5; // ms, unrelated to the 100-micro price
    let r = Router::new(vec![desc("p", "m", true, 100_000, 4096, e)]);
    let req = RouteRequest {
        context_tokens: 1,
        estimated_output_tokens: 0,
        task_budget_remaining_micro: 99,
        ..Default::default()
    };
    let err = r.route(&req, &[]).unwrap_err();
    assert!(err.contains("budget"), "{err}");
}

// ==================================================================
// Verified-outcome learning integration (audit items 13/14/L): scoring
// consults conservative WorkCostEstimates only when verified history
// exists; an empty registry is byte-identical to the legacy router.
// ==================================================================

fn implement_req(tokens_in: u64, tokens_out: u64, floor: u8) -> RouteRequest {
    RouteRequest {
        phase: RouterPhase::Implement,
        required_capabilities: caps(&["tools", "streaming"]),
        context_tokens: tokens_in,
        estimated_output_tokens: tokens_out,
        quality_floor: floor,
        quality_target: None,
        task_budget_remaining_micro: 0,
        latency_preference_ms: None,
        task_class: TaskClass::Medium,
        risk_bucket: RiskBucket::Low,
    }
}

fn sample(verified_success: bool, rework: u64) -> OutcomeSample {
    OutcomeSample {
        verified_success,
        rework_cost_micro: rework,
        rework_turns: 1,
    }
}

#[test]
fn empty_outcome_registry_is_byte_identical_to_legacy_routing() {
    // The additive guarantee: a service built with an empty outcome
    // store makes EXACTLY the decisions of the pre-outcome constructors
    // (same candidate set, same request, including the audit strings).
    let candidates = vec![
        desc("cheap", "fast", true, 512_000, 64_000, econ(1, 3, 82, 82)),
        desc("f1", "big", true, 512_000, 64_000, econ(15, 60, 95, 95)),
    ];
    let legacy = RouterService::new(candidates.clone());
    let empty_registry = RouterService::with_outcomes(candidates, Arc::new(EmptyOutcomeStore));
    let req = implement_req(40_000, 6_000, 80);
    for _ in 0..3 {
        let a = legacy.route(&req, &[]).unwrap();
        let b = empty_registry.route(&req, &[]).unwrap();
        assert_eq!(a, b, "empty outcome registry must not change decisions");
        assert!(!a.reasoning.contains("rework_ppm"), "{}", a.reasoning);
    }
}

#[test]
fn verified_rework_history_flips_economy_to_the_strong_model() {
    // Cheap model: base 58_000 micro on the request, observed 45%
    // rework over 200 verified samples (each failure's downstream spend
    // measured at 2.4M). Strong model: base 960_000, observed 6%
    // rework over 100 samples (measured 960k per failure). Totals favor
    // the STRONG model after the verified history exists; the legacy
    // expected-cost prior (no stats) favors the cheap model.
    let cheap = desc("e1", "cheap", true, 512_000, 64_000, econ(1, 3, 82, 82));
    let strong = desc("f1", "big", true, 512_000, 64_000, econ(15, 60, 95, 95));
    let candidates = vec![cheap, strong];
    let req = implement_req(40_000, 6_000, 80);
    let before = RouterService::new(candidates.clone())
        .route(&req, &[])
        .unwrap();
    assert_eq!(
        (before.provider.as_str(), before.model.as_str()),
        ("e1", "cheap"),
        "without verified history the cheap model's expected cost wins: {}",
        before.reasoning
    );
    // Verified samples under several class/risk keys of the Implement
    // phase: the route consult folds them per phase.
    let store = MemoryOutcomeStore::new();
    for (class, bucket, ok, fail) in [
        (TaskClass::Medium, RiskBucket::Low, 55u64, 45u64),
        (TaskClass::Hard, RiskBucket::High, 55, 45),
    ] {
        for _ in 0..ok {
            store.append_sample(
                &OutcomeKey {
                    provider: "e1".into(),
                    model: "cheap".into(),
                    phase: RouterPhase::Implement,
                    task_class: class,
                    risk_bucket: bucket,
                },
                sample(true, 0),
            );
        }
        for _ in 0..fail {
            store.append_sample(
                &OutcomeKey {
                    provider: "e1".into(),
                    model: "cheap".into(),
                    phase: RouterPhase::Implement,
                    task_class: class,
                    risk_bucket: bucket,
                },
                sample(false, 2_400_000),
            );
        }
    }
    for _ in 0..94 {
        store.append_sample(
            &OutcomeKey {
                provider: "f1".into(),
                model: "big".into(),
                phase: RouterPhase::Implement,
                task_class: TaskClass::Medium,
                risk_bucket: RiskBucket::Low,
            },
            sample(true, 0),
        );
    }
    for _ in 0..6 {
        store.append_sample(
            &OutcomeKey {
                provider: "f1".into(),
                model: "big".into(),
                phase: RouterPhase::Implement,
                task_class: TaskClass::Medium,
                risk_bucket: RiskBucket::Low,
            },
            sample(false, 960_000),
        );
    }
    let svc = RouterService::with_outcomes(candidates, Arc::new(store));
    let after = svc.route(&req, &[]).unwrap();
    assert_eq!(
        (after.provider.as_str(), after.model.as_str()),
        ("f1", "big"),
        "verified rework history must flip Economy to the strong model: {}",
        after.reasoning
    );
    assert!(
        after.reasoning.contains("rework_ppm="),
        "the audit string must name the verified estimate: {}",
        after.reasoning
    );
    // Deterministic over the same history.
    let again = svc.route(&req, &[]).unwrap();
    assert_eq!(after, again);
}

#[test]
fn two_verified_successes_stay_conservative_and_never_license_cheap() {
    // The audit's small-sample rule at the ROUTER level: a candidate
    // with TWO verified first-pass successes must NOT be treated as a
    // zero-rework license. Trusting 2/2 as excellent would make the
    // 100k-micro candidate win (100k < the fresh candidate's ~175k
    // legacy expected cost); the conservative upper rework bound keeps
    // its expected verified cost at ~200k, so the fresh 150k candidate
    // wins.
    let two_time = desc("p1", "proven2x", true, 512_000, 64_000, {
        let mut e = econ(4, 30, 88, 88);
        e.estimated_latency_ms = 500;
        e
    });
    let fresh = desc("p2", "fresh", true, 512_000, 64_000, {
        let mut e = econ(10, 25, 88, 88);
        e.estimated_latency_ms = 500;
        e
    });
    let candidates = vec![two_time.clone(), fresh.clone()];
    let req = implement_req(10_000, 2_000, 60);
    // 100k base (10k x 4 + 2k x 30) vs 150k base (10k x 10 + 2k x 25).
    assert_eq!(base_call_cost(&two_time, &req, &[]), 100_000);
    assert_eq!(base_call_cost(&fresh, &req, &[]), 150_000);
    let store = MemoryOutcomeStore::new();
    for _ in 0..2 {
        store.append_sample(
            &OutcomeKey {
                provider: "p1".into(),
                model: "proven2x".into(),
                phase: RouterPhase::Implement,
                task_class: TaskClass::Medium,
                risk_bucket: RiskBucket::Low,
            },
            sample(true, 0),
        );
    }
    let two_stats = store
        .phase_stats("p1", "proven2x", RouterPhase::Implement)
        .unwrap();
    assert!(
        verified_success_confidence_ppm(&two_stats) < outcomes::EXCELLENT_CONFIDENCE_PPM,
        "2/2 must stay below the excellent bar"
    );
    let svc = RouterService::with_outcomes(candidates, Arc::new(store));
    let d = svc.route(&req, &[]).unwrap();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        ("p2", "fresh"),
        "two verified successes must NOT license the cheaper candidate: {}",
        d.reasoning
    );
    // Prove the licensing arithmetic: under a zero-rework reading the
    // 2/2 candidate would win (100k base), but the conservative Wilson
    // upper rework bound keeps its expected verified cost above the
    // fresh candidate's legacy expected cost.
    let health = LiveHealth::default();
    let qualified = qualified_candidates(&svc.router.candidates, &req, &[], &health).unwrap();
    let scored = score_candidates(&qualified, req.phase, svc.outcomes.as_ref());
    let proven = scored
        .iter()
        .find(|s| s.candidate.provider == "p1")
        .unwrap();
    let fresh_scored = scored
        .iter()
        .find(|s| s.candidate.provider == "p2")
        .unwrap();
    assert_eq!(proven.call_cost_micro, 100_000);
    assert_eq!(proven.expected_cost_micro, 200_001);
    assert_eq!(fresh_scored.expected_cost_micro, 175_000);
    assert!(
        proven.expected_cost_micro > fresh_scored.expected_cost_micro,
        "2/2 must not clear the excellent bar: proven {} vs fresh {}",
        proven.expected_cost_micro,
        fresh_scored.expected_cost_micro
    );
}

#[test]
fn failed_verification_is_never_learned_as_a_success_and_hostile_stats_saturate() {
    // Verified-only attribution at the registry level: three "model
    // said done" calls that failed verification record three FAILURES
    // with their rework, and zero successes — routing over that history
    // keeps the strong candidate's cost honest (never zeroed).
    let strong = desc("f1", "big", true, 512_000, 64_000, econ(15, 60, 95, 95));
    let cheap = desc("e1", "cheap", true, 512_000, 64_000, econ(1, 3, 82, 82));
    let store = MemoryOutcomeStore::new();
    for _ in 0..3 {
        store.append_sample(
            &OutcomeKey {
                provider: "f1".into(),
                model: "big".into(),
                phase: RouterPhase::Implement,
                task_class: TaskClass::Hard,
                risk_bucket: RiskBucket::High,
            },
            sample(false, 960_000),
        );
    }
    let st = store
        .stats(&OutcomeKey {
            provider: "f1".into(),
            model: "big".into(),
            phase: RouterPhase::Implement,
            task_class: TaskClass::Hard,
            risk_bucket: RiskBucket::High,
        })
        .unwrap();
    assert_eq!(st.successes_first_pass, 0, "no success may be learned");
    assert_eq!(st.failures_first_pass, 3);
    assert_eq!(st.rework_cost_micro_sum, 3 * 960_000);
    // Hostile store rows must saturate, never panic or understate: a
    // registry that reports near-maximum rework spend keeps the
    // candidate's estimate at the saturation ceiling.
    struct Hostile;
    impl OutcomeView for Hostile {
        fn stats(&self, _key: &OutcomeKey) -> Option<VerifiedOutcomeStats> {
            Some(VerifiedOutcomeStats {
                failures_first_pass: 1,
                rework_cost_micro_sum: u64::MAX,
                sample_count: 1,
                ..Default::default()
            })
        }
        fn phase_stats(
            &self,
            _provider: &str,
            _model: &str,
            _phase: RouterPhase,
        ) -> Option<VerifiedOutcomeStats> {
            Some(VerifiedOutcomeStats {
                failures_first_pass: 1,
                rework_cost_micro_sum: u64::MAX,
                sample_count: 1,
                ..Default::default()
            })
        }
    }
    impl OutcomeStore for Hostile {
        fn append_sample(&self, _key: &OutcomeKey, _s: OutcomeSample) {}
    }
    let svc = RouterService::with_outcomes(vec![cheap, strong], Arc::new(Hostile));
    let d = svc.route(&implement_req(40_000, 6_000, 80), &[]).unwrap();
    assert_eq!(
        d.estimated_cost_micro, 58_000,
        "decision cost stays the base"
    );
    assert!(
        d.reasoning.contains("total_expected_micro="),
        "saturating hostile stats must not panic and must stay auditable: {}",
        d.reasoning
    );
}

// ------------------------------------------ candidate-specific sizing

/// The two-candidate fixture of the audit: the SAME prompt is 10k tokens
/// under A (20k window) and 13k under B (12k window) — the cheap
/// qualification pass admits both, but B's REAL footprint overflows its
/// own window and must be filtered before final selection.
#[test]
fn real_footprint_filter_drops_a_candidate_that_overflows_its_window() {
    let a = desc("a", "am", true, 20_000, 4096, econ(1, 2, 90, 90));
    let b = desc("b", "bm", true, 12_000, 4096, econ(1, 2, 90, 90));
    let qualified = vec![
        QualifiedCandidate {
            descriptor: &a,
            call_cost_micro: 10,
            success_ppm: 900_000,
            cost: CostEstimate::Conservative(10),
        },
        QualifiedCandidate {
            descriptor: &b,
            call_cost_micro: 10,
            success_ppm: 900_000,
            cost: CostEstimate::Conservative(10),
        },
    ];
    let sizes: std::collections::HashMap<&str, u64> =
        [("am", 10_000u64), ("bm", 13_000u64)].into_iter().collect();
    let mut sized_calls = 0usize;
    let survivors = size_candidates_top_k(&qualified, 8, |d| {
        sized_calls += 1;
        (sizes[d.model.as_str()], true)
    });
    assert_eq!(
        sized_calls, 2,
        "each seriously-considered candidate sized once"
    );
    assert_eq!(
        survivors.len(),
        1,
        "B's 13k footprint overflows its 12k window"
    );
    assert_eq!(survivors[0].candidate.model, "am");
    assert_eq!(survivors[0].input_tokens, 10_000);
    assert!(survivors[0].exact);
}

#[test]
fn sizing_work_is_bounded_to_the_top_k_and_order_is_preserved() {
    let models: Vec<ModelDescriptor> = (0..20)
        .map(|i| {
            desc(
                "p",
                &format!("m{i:02}"),
                true,
                100_000,
                4096,
                econ(1, 2, 90, 90),
            )
        })
        .collect();
    let qualified: Vec<QualifiedCandidate<'_>> = models
        .iter()
        .map(|d| QualifiedCandidate {
            descriptor: d,
            call_cost_micro: 10,
            success_ppm: 900_000,
            cost: CostEstimate::Conservative(10),
        })
        .collect();
    let mut seen: Vec<String> = Vec::new();
    let survivors = size_candidates_top_k(&qualified, 64, |d| {
        seen.push(d.model.clone());
        (1_000, true)
    });
    assert_eq!(
        seen.len(),
        MAX_SIZED_CANDIDATES,
        "the sizer must never run more than the hard top-K cap"
    );
    assert_eq!(survivors.len(), MAX_SIZED_CANDIDATES);
    assert_eq!(survivors[0].candidate.model, "m00");
    assert_eq!(survivors[MAX_SIZED_CANDIDATES - 1].candidate.model, "m07");
    // A caller asking for fewer sizes fewer (bound includes 0).
    assert!(size_candidates_top_k(&qualified, 0, |_| (1, true)).is_empty());
    let three = size_candidates_top_k(&qualified, 3, |_| (1, true));
    assert_eq!(three.len(), 3);
}

#[test]
fn every_survivor_that_is_not_sized_is_never_returned_unsized() {
    // An EMPTY survivor set is the honest end state when the top-K all
    // overflow; the helper must not fall through to an unsized pick.
    let a = desc("a", "am", true, 5_000, 4096, econ(1, 2, 90, 90));
    let qualified = vec![QualifiedCandidate {
        descriptor: &a,
        call_cost_micro: 10,
        success_ppm: 900_000,
        cost: CostEstimate::Conservative(10),
    }];
    let survivors = size_candidates_top_k(&qualified, 8, |_| (5_001, false));
    assert!(survivors.is_empty());
    // Exactly at the window boundary fits (<=, not <).
    let at_boundary = size_candidates_top_k(&qualified, 8, |_| (5_000, true));
    assert_eq!(at_boundary.len(), 1);
}

/// Deterministic test planner: fixed footprints per (provider, model),
/// a unique build serial per invocation, and a build log. The payload is
/// an opaque `String` (the router must not care what the plan is).
struct FixedPlanner {
    sizes: std::collections::HashMap<(String, String), (u64, bool)>,
    builds: std::cell::RefCell<Vec<String>>,
    built_payloads: std::cell::RefCell<std::collections::HashMap<String, String>>,
    serial: std::cell::Cell<u64>,
}

impl FixedPlanner {
    fn new(sizes: &[(&str, &str, u64, bool)]) -> Self {
        Self {
            sizes: sizes
                .iter()
                .map(|(p, m, t, e)| ((p.to_string(), m.to_string()), (*t, *e)))
                .collect(),
            builds: std::cell::RefCell::new(Vec::new()),
            built_payloads: std::cell::RefCell::new(std::collections::HashMap::new()),
            serial: std::cell::Cell::new(0),
        }
    }

    fn builds(&self) -> Vec<String> {
        self.builds.borrow().clone()
    }

    fn payload_for(&self, model: &str) -> Option<String> {
        self.built_payloads.borrow().get(model).cloned()
    }
}

impl CandidatePlanner for FixedPlanner {
    fn build(&self, descriptor: &ModelDescriptor) -> Option<BuiltCandidatePlan> {
        self.builds.borrow_mut().push(descriptor.model.clone());
        let (tokens, exact) = *self
            .sizes
            .get(&(descriptor.provider.clone(), descriptor.model.clone()))?;
        let serial = self.serial.get() + 1;
        self.serial.set(serial);
        let payload = format!("plan:{}#{serial}", descriptor.model);
        self.built_payloads
            .borrow_mut()
            .insert(descriptor.model.clone(), payload.clone());
        Some(BuiltCandidatePlan {
            wire_plan: Box::new(payload),
            footprint: CandidateFootprint {
                input_tokens: tokens,
                exact,
            },
        })
    }
}

/// THE audit scenario: the cheap pre-rank sees the PLANNING model's 10k
/// count and B (window 12k) is otherwise cheapest/best, but B's own
/// tokenizer measures 13k — B must be dropped on its REAL footprint and
/// A must win. The winner's plan is the one built during sizing (the
/// same serial), and no candidate is built twice.
#[test]
fn sized_route_drops_a_cheapest_candidate_that_overflows_its_own_window() {
    let a = candidate(
        perf_desc("a", "am", true, 20_000, 4096, 90, 90),
        exact_pricing(1_000_000, 0),
    );
    let b = candidate(
        perf_desc("b", "bm", true, 12_000, 4096, 99, 99),
        exact_pricing(100_000, 0),
    );
    let svc = RouterService::with_route_candidates(vec![a, b], Arc::new(EmptyOutcomeStore));
    let planner = FixedPlanner::new(&[("a", "am", 10_000, true), ("b", "bm", 13_000, false)]);
    // The pre-rank input: the PLANNING model's count (10k) admits both.
    let req = priced_req(10_000, 0, 50);
    let sized = svc.route_with_candidate_plans(&req, &[], &planner).unwrap();
    assert_eq!(
        (
            sized.decision.provider.as_str(),
            sized.decision.model.as_str()
        ),
        ("a", "am"),
        "B's real 13k footprint overflows its 12k window: {}",
        sized.decision.reasoning
    );
    assert!(
        sized
            .decision
            .reasoning
            .contains("sized_input=10000 sized_exact=true"),
        "the audit string must show the measured footprint and honesty: {}",
        sized.decision.reasoning
    );
    // Every seriously-considered candidate was sized exactly once, in
    // pre-rank order (B was cheaper under the unmeasured count).
    assert_eq!(planner.builds(), vec!["bm".to_string(), "am".to_string()]);
    // The returned plan IS the plan built for the winner during sizing
    // (same serial) — never rebuilt after selection.
    let plan = sized.plan.expect("a sized route carries the winner's plan");
    assert_eq!(plan.candidate.model, "am");
    assert_eq!(plan.footprint.input_tokens, 10_000);
    assert!(plan.footprint.exact);
    let payload = plan
        .wire_plan
        .downcast::<String>()
        .expect("the test planner's payload");
    assert_eq!(
        Some(*payload),
        planner.payload_for("am"),
        "the winning plan must be the plan already built during sizing"
    );
}

/// Tokenizer differences change the Economy winner through DOLLAR cost:
/// A has the lower unit price but its own tokenizer measures 13k; B's
/// unit price is higher but its tokenizer measures 10k, so the re-price
/// over the measured footprint flips the winner to B.
#[test]
fn measured_footprint_can_flip_the_economy_winner_on_dollar_cost() {
    let a = candidate(
        perf_desc("a", "am", true, 100_000, 4096, 90, 90),
        exact_pricing(800_000, 0),
    );
    let b = candidate(
        perf_desc("b", "bm", true, 100_000, 4096, 90, 90),
        exact_pricing(1_000_000, 0),
    );
    let svc = RouterService::with_route_candidates(vec![a, b], Arc::new(EmptyOutcomeStore));
    let planner = FixedPlanner::new(&[("a", "am", 13_000, true), ("b", "bm", 10_000, true)]);
    let req = priced_req(12_000, 0, 50);
    let sized = svc.route_with_candidate_plans(&req, &[], &planner).unwrap();
    assert_eq!(
        (
            sized.decision.provider.as_str(),
            sized.decision.model.as_str(),
            sized.decision.estimated_cost_micro
        ),
        ("b", "bm", 10_000),
        "10k at $1/M beats 13k at $0.80/M: {}",
        sized.decision.reasoning
    );
    // The pre-rank (unmeasured 12k) built A first — the cheap $0.80/M
    // unit price — and only the measured re-price flipped the winner.
    assert_eq!(planner.builds(), vec!["am".to_string(), "bm".to_string()]);
    let plan = sized.plan.expect("a sized route carries the winner's plan");
    assert_eq!(plan.candidate.model, "bm");
    assert_eq!(plan.call_cost, CostEstimate::Known(10_000));
}

/// A sized route whose top-K ALL overflow their own windows is a typed
/// refusal naming the fit axis — never an unsized fallback pick.
#[test]
fn sized_route_refuses_when_every_candidate_overflows_its_own_window() {
    let a = candidate(
        perf_desc("a", "am", true, 5_000, 4096, 90, 90),
        exact_pricing(1_000_000, 0),
    );
    let svc = RouterService::with_route_candidates(vec![a], Arc::new(EmptyOutcomeStore));
    let planner = FixedPlanner::new(&[("a", "am", 5_001, true)]);
    let req = priced_req(1_000, 0, 50);
    let err = svc
        .route_with_candidate_plans(&req, &[], &planner)
        .err()
        .expect("all-overflow must refuse");
    assert!(err.contains("capability/fit"), "{err}");
}

/// An unusable plan (`None`) is a dropped candidate, never a survivor
/// with a fabricated footprint; a second usable candidate still wins.
#[test]
fn planner_returning_none_drops_the_candidate_without_faking_a_footprint() {
    let a = candidate(
        perf_desc("a", "am", true, 100_000, 4096, 90, 90),
        exact_pricing(100_000, 0),
    );
    let b = candidate(
        perf_desc("b", "bm", true, 100_000, 4096, 90, 90),
        exact_pricing(1_000_000, 0),
    );
    let svc = RouterService::with_route_candidates(vec![a, b], Arc::new(EmptyOutcomeStore));
    // Only B has a size row: A's build returns None.
    let planner = FixedPlanner::new(&[("b", "bm", 1_000, true)]);
    let req = priced_req(1_000, 0, 50);
    let sized = svc.route_with_candidate_plans(&req, &[], &planner).unwrap();
    assert_eq!(sized.decision.model, "bm");
}

// ==================================================================
// Priced-path integration (pricing-path audit): RouteCandidate +
// PricingState as the router's priced unit, the exact per-million quote
// as the only money math, and unknown as a NON-numeric state.
// ==================================================================

use faktor_core::model::{MicroUsdPerMillionTokens, PriceQuote};

fn perf_desc(
    provider: &str,
    model: &str,
    tools: bool,
    context: u64,
    out: u64,
    coding: u8,
    context_rel: u8,
) -> ModelDescriptor {
    let mut e = econ(0, 0, coding, coding);
    e.context_reliability = context_rel;
    desc(provider, model, tools, context, out, e)
}

fn candidate(d: ModelDescriptor, pricing: PricingState) -> RouteCandidate {
    RouteCandidate::new(d, pricing)
}

fn exact_quote(input: u64, output: u64) -> PriceQuote {
    PriceQuote {
        input: MicroUsdPerMillionTokens(input),
        output: MicroUsdPerMillionTokens(output),
        cache_read: MicroUsdPerMillionTokens(0),
        cache_write: MicroUsdPerMillionTokens(0),
    }
}

fn exact_pricing(input: u64, output: u64) -> PricingState {
    PricingState::Known(PricingSnapshot::exact(
        exact_quote(input, output),
        1,
        "priced-test".into(),
    ))
}

fn priced_req(context_tokens: u64, output_tokens: u64, floor: u8) -> RouteRequest {
    RouteRequest {
        phase: RouterPhase::Implement,
        required_capabilities: caps(&["tools"]),
        context_tokens,
        estimated_output_tokens: output_tokens,
        quality_floor: floor,
        quality_target: None,
        task_budget_remaining_micro: 0,
        latency_preference_ms: None,
        task_class: TaskClass::Medium,
        risk_bucket: RiskBucket::Low,
    }
}

#[test]
fn router_uses_exact_sub_dollar_quotes() {
    // $0.10/M vs $0.15/M input are BOTH 1 microUSD by the legacy
    // per-token ceil projection — indistinguishable. The exact
    // per-million quote prices them 100_000 vs 150_000 micro on 1M
    // tokens, so the router MUST rank them distinctly through the
    // exact math, never through the lossy projection.
    let cheap = candidate(
        perf_desc("a", "cheap", true, 2_000_000, 64_000, 90, 90),
        exact_pricing(100_000, 0),
    );
    let rich = candidate(
        perf_desc("b", "rich", true, 2_000_000, 64_000, 90, 90),
        exact_pricing(150_000, 0),
    );
    let svc = RouterService::with_route_candidates(
        vec![cheap.clone(), rich.clone()],
        Arc::new(EmptyOutcomeStore),
    );
    let req = priced_req(1_000_000, 0, 50);
    let decision = svc.route(&req, &[]).unwrap();
    assert_eq!(
        (decision.provider.as_str(), decision.model.as_str()),
        ("a", "cheap"),
        "exact quotes must rank $0.10/M below $0.15/M: {}",
        decision.reasoning
    );
    assert_eq!(decision.estimated_cost_micro, 100_000);
    let snap = decision
        .pricing_snapshot
        .expect("priced decisions carry snapshots");
    assert_eq!(snap.authority, PriceAuthority::Exact);
    assert_eq!(snap.settle_cost(1_000_000, 0, 0, 0), Some(100_000));

    // The sub-dollar regime at small token counts: 100 micro vs 150
    // micro — the projection would round both to 1 micro/token.
    let small = priced_req(1_000, 0, 50);
    assert_eq!(
        CostEstimate::from_state(&cheap.pricing, TokenUsage::new(1_000, 0, 0, 0)),
        CostEstimate::Known(100)
    );
    assert_eq!(
        CostEstimate::from_state(&rich.pricing, TokenUsage::new(1_000, 0, 0, 0)),
        CostEstimate::Known(150)
    );
    let d = svc.route(&small, &[]).unwrap();
    assert_eq!(d.provider, "a");
    assert_eq!(d.estimated_cost_micro, 100, "exact sub-dollar quote");
}

/// Seeded numerical-recipe LCG (identical on every platform).
struct QuoteGen {
    state: u64,
}

impl QuoteGen {
    fn new(seed: u64) -> Self {
        Self {
            state: seed
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0x1234_5678_9ABC_DEF0),
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.state >> 33) as u32 as u64
    }

    fn range(&mut self, lo: u64, hi_exclusive: u64) -> u64 {
        assert!(hi_exclusive > lo);
        lo + self.next() % (hi_exclusive - lo)
    }
}

#[test]
fn router_budget_matches_quote_oracle() {
    // 10,000 randomized cases: the router's budget admission is checked
    // against an ORACLE re-implementation of the exact per-million quote
    // (`PriceQuote::quote_cost_micro`). exact == cap admits, a cap
    // above the exact cost admits, a cap below it rejects; Unknown is
    // admitted ONLY without a cap; LocalZero is exactly zero.
    let mut g = QuoteGen::new(0xB0D6_E7A5);
    let mut examined = 0usize;
    for _ in 0..10_000 {
        let quote = PriceQuote {
            input: MicroUsdPerMillionTokens(g.range(0, 5_000_000)),
            output: MicroUsdPerMillionTokens(g.range(0, 20_000_000)),
            cache_read: MicroUsdPerMillionTokens(g.range(0, 1_000_000)),
            cache_write: MicroUsdPerMillionTokens(g.range(0, 1_000_000)),
        };
        let ctx = g.range(0, 200_000);
        let out = g.range(0, 20_000);
        let usage = TokenUsage::new(ctx, 0, 0, out);
        let oracle = quote.quote_cost_micro(usage);
        let pricing = match g.range(0, 4) {
            0 => PricingState::Known(PricingSnapshot::exact(quote, 1, "oracle".into())),
            1 => PricingState::ConservativeCeiling(PricingSnapshot::conservative_ceiling(
                quote,
                1,
                "oracle".into(),
            )),
            2 => PricingState::LocalZero,
            _ => PricingState::Unknown,
        };
        let c = candidate(
            perf_desc("p", "m", true, 1_000_000, 64_000, 90, 90),
            pricing.clone(),
        );
        let admit = |budget: u64| {
            qualified_priced_candidates(
                std::slice::from_ref(&c),
                &RouteRequest {
                    task_budget_remaining_micro: budget,
                    ..priced_req(ctx, out, 50)
                },
                &[],
                &LiveHealth::default(),
            )
            .is_ok()
        };
        match &pricing {
            PricingState::Unknown => {
                assert!(admit(0), "Unknown is admitted while no cap exists");
                assert!(
                    !admit(1),
                    "Unknown fails closed the moment a hard cap exists"
                );
            }
            PricingState::LocalZero => {
                assert!(admit(1), "the authoritative zero fits every positive cap");
                let q = qualified_priced_candidates(
                    std::slice::from_ref(&c),
                    &RouteRequest {
                        task_budget_remaining_micro: 1,
                        ..priced_req(ctx, out, 50)
                    },
                    &[],
                    &LiveHealth::default(),
                )
                .unwrap();
                assert_eq!(q[0].cost, CostEstimate::LocalZero);
                assert_eq!(q[0].cost.numeric(), Some(0));
            }
            _ => {
                let estimate = CostEstimate::from_state(&pricing, usage);
                assert_eq!(
                    estimate.numeric(),
                    Some(oracle),
                    "router estimate diverged from the quote oracle"
                );
                assert!(admit(oracle), "cost == cap must admit");
                assert!(admit(oracle + 1), "cap above the exact cost must admit");
                if oracle > 0 {
                    assert!(
                        !admit(oracle - 1),
                        "a cap one micro below the exact cost must reject"
                    );
                }
                examined += 1;
            }
        }
    }
    assert!(examined > 4_000, "the case mix must exercise real quotes");
}

#[test]
fn maximum_quality_unknown_without_cap() {
    // The best model is Unknown-priced: without a hard cap it must not
    // be excluded (quality decides), and under a hard cap it must fail
    // closed rather than be treated as free.
    let best = candidate(
        perf_desc("best", "bm", true, 256_000, 64_000, 95, 95),
        PricingState::Unknown,
    );
    let affordable = candidate(
        perf_desc("ok", "om", true, 256_000, 64_000, 80, 80),
        exact_pricing(1_000_000, 3_000_000),
    );
    let svc = RouterService::with_route_candidates(
        vec![best.clone(), affordable.clone()],
        Arc::new(EmptyOutcomeStore),
    );
    let req = priced_req(1_000, 100, 90);
    let both = [best.clone(), affordable.clone()];
    let qualified = qualified_priced_candidates(&both, &req, &[], &LiveHealth::default()).unwrap();
    assert_eq!(qualified.len(), 1, "only the 95-quality model clears 90");
    assert_eq!(qualified[0].cost, CostEstimate::Unknown);
    let d = svc.route(&req, &[]).unwrap();
    assert_eq!((d.provider.as_str(), d.model.as_str()), ("best", "bm"));
    let snap = d.pricing_snapshot.expect("decision carries the snapshot");
    assert_eq!(snap.authority, PriceAuthority::Unknown);
    assert_eq!(snap.settle_cost(1_000, 0, 0, 100), None);
    assert!(
        d.reasoning.contains("cost=unknown"),
        "the decision must SAY it is unpriced: {}",
        d.reasoning
    );

    // Hard cap: the Unknown best is excluded; the affordable tier clears
    // the floor and serves.
    let mut capped = req.clone();
    capped.quality_floor = 50;
    capped.task_budget_remaining_micro = 1_000_000_000;
    let d = svc.route(&capped, &[]).unwrap();
    assert_eq!(
        (d.provider.as_str(), d.model.as_str()),
        ("ok", "om"),
        "under a hard cap the unpriced best is not treated as free: {}",
        d.reasoning
    );

    // Hard cap and NO affordable above-floor candidate: typed refusal.
    let only_best = RouterService::with_route_candidates(vec![best], Arc::new(EmptyOutcomeStore));
    let mut capped_high_floor = capped.clone();
    capped_high_floor.quality_floor = 90;
    assert!(
        only_best.route(&capped_high_floor, &[]).is_err(),
        "an unpriced candidate must never fit a hard cap"
    );
}

#[test]
fn pinned_never_competes() {
    // Pinned routing calls ONLY qualify_specific: the pin wins even
    // when the free evaluation would pick a much cheaper model.
    let pin = candidate(
        perf_desc("pin", "pm", true, 256_000, 64_000, 90, 90),
        exact_pricing(50_000_000, 150_000_000),
    );
    let cheap = candidate(
        perf_desc("cheap", "cm", true, 256_000, 64_000, 90, 90),
        exact_pricing(1_000, 2_000),
    );
    let free = RouterService::with_route_candidates(
        vec![pin.clone(), cheap.clone()],
        Arc::new(EmptyOutcomeStore),
    );
    let req = priced_req(1_000, 100, 50);
    assert_eq!(free.route(&req, &[]).unwrap().provider, "cheap");

    let pinned = RouterService::with_pinned_route_candidates(
        vec![pin, cheap],
        "pin",
        "pm",
        Arc::new(EmptyOutcomeStore),
    );
    let d = pinned.route(&req, &[]).unwrap();
    assert_eq!((d.provider.as_str(), d.model.as_str()), ("pin", "pm"));
    let snap = d.pricing_snapshot.expect("pin snapshots");
    assert_eq!(snap.authority, PriceAuthority::Exact);
    assert_eq!(snap.settle_cost(1_000_000, 0, 0, 0), Some(50_000_000));
    assert!(
        d.reasoning.contains("pinned"),
        "the pinned decision is auditable: {}",
        d.reasoning
    );
}

#[test]
fn pin_lacking_tools_is_a_no_capable_model_refusal() {
    // Pinned qualification checks the ONE pin; a pin missing a required
    // capability stages the capability failure (NoCapableModel), never
    // a silent substitution or a lowered requirement.
    let pin = candidate(
        perf_desc("pin", "pm", false, 256_000, 64_000, 95, 95),
        PricingState::Unknown,
    );
    let req = priced_req(1_000, 100, 50);
    let failure = qualify_specific(
        std::slice::from_ref(&pin),
        "pin",
        "pm",
        &req,
        &[],
        &LiveHealth::default(),
    )
    .expect_err("a tool-less pin cannot serve a tools request");
    assert_eq!(failure.fit_survivors, 0);
    assert!(
        failure
            .unsupported_capabilities
            .iter()
            .any(|c| c == "tools"),
        "{failure:?}"
    );
    assert!(
        failure
            .route_error()
            .contains("no candidate clears capability/fit"),
        "{}",
        failure.route_error()
    );
    let svc = RouterService::with_pinned_route_candidates(
        vec![pin],
        "pin",
        "pm",
        Arc::new(EmptyOutcomeStore),
    );
    let err = svc.route(&req, &[]).unwrap_err();
    assert!(err.contains("no candidate clears capability/fit"), "{err}");
}

#[test]
fn pin_unknown_is_accepted_without_cap_and_refused_under_a_hard_cap() {
    let pin = candidate(
        perf_desc("pin", "pm", true, 256_000, 64_000, 90, 90),
        PricingState::Unknown,
    );
    let svc = RouterService::with_pinned_route_candidates(
        vec![pin],
        "pin",
        "pm",
        Arc::new(EmptyOutcomeStore),
    );
    let req = priced_req(1_000, 100, 50);
    let d = svc.route(&req, &[]).unwrap();
    assert_eq!((d.provider.as_str(), d.model.as_str()), ("pin", "pm"));
    assert_eq!(
        d.pricing_snapshot.expect("snapshot").authority,
        PriceAuthority::Unknown
    );
    // A hard cap cannot reserve an unknown price: the pin fails closed
    // (no fabricated number), never "treated as free".
    let mut capped = req;
    capped.task_budget_remaining_micro = 10_000_000_000;
    let err = svc.route(&capped, &[]).unwrap_err();
    assert!(
        err.contains("budget/latency"),
        "unknown under a hard cap is a budget refusal: {err}"
    );
}

#[test]
fn local_zero_is_exactly_zero_by_authority() {
    let local = candidate(
        perf_desc("local", "lm", true, 2_000_000, 64_000, 90, 90),
        PricingState::LocalZero,
    );
    let req = priced_req(1_000_000, 10_000, 50);
    let qualified = qualified_priced_candidates(
        std::slice::from_ref(&local),
        &req,
        &[],
        &LiveHealth::default(),
    )
    .unwrap();
    assert_eq!(qualified[0].cost, CostEstimate::LocalZero);
    assert_eq!(qualified[0].cost.numeric(), Some(0));
    let svc = RouterService::with_route_candidates(vec![local], Arc::new(EmptyOutcomeStore));
    let d = svc.route(&req, &[]).unwrap();
    assert_eq!(d.estimated_cost_micro, 0, "an authoritative zero is zero");
    let snap = d.pricing_snapshot.expect("snapshot");
    assert_eq!(snap.authority, PriceAuthority::LocalZero);
    assert_eq!(
        snap.settle_cost(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
        Some(0)
    );
}

#[test]
fn unknown_cost_is_never_a_numeric_zero() {
    let usage = TokenUsage::new(1_000, 0, 0, 100);
    assert_eq!(CostEstimate::Unknown.numeric(), None);
    assert_ne!(CostEstimate::Unknown, CostEstimate::Known(0));
    assert_ne!(CostEstimate::Unknown, CostEstimate::LocalZero);
    assert!(
        !matches!(CostEstimate::Unknown.numeric(), Some(0)),
        "unknown must not read as free"
    );
    // Unknown always ranks AFTER every numeric cost (including 0).
    assert_eq!(
        cmp_optional_cost(CostEstimate::Unknown.numeric(), Some(0)),
        std::cmp::Ordering::Greater
    );
    // A hostile zero-by-number quote under Unknown authority stays
    // Unknown and settles to NOTHING.
    let mut hostile = PricingSnapshot::unknown(1, "hostile".into());
    hostile.quote = Some(PriceQuote::ZERO);
    assert_eq!(
        CostEstimate::from_snapshot(&hostile, usage),
        CostEstimate::Unknown
    );
    // A `Known` snapshot with a ZERO quote is a real (exact) zero-price
    // statement, not a local runtime: authority is the variant.
    let zero_known = PricingSnapshot::exact(PriceQuote::ZERO, 1, "zero-price".into());
    assert_eq!(
        CostEstimate::from_snapshot(&zero_known, usage),
        CostEstimate::Known(0)
    );
}

#[test]
fn priced_outcomes_dominate_priors_when_present() {
    // Durable verified history dominates the conservative priors: a
    // cheap model with measured rework loses to a reliable model once
    // the history exists, on the priced path too.
    let cheap = candidate(
        perf_desc("e1", "cheap", true, 512_000, 64_000, 82, 82),
        exact_pricing(1_000_000, 3_000_000),
    );
    let strong = candidate(
        perf_desc("f1", "big", true, 512_000, 64_000, 95, 95),
        exact_pricing(15_000_000, 60_000_000),
    );
    let store = Arc::new(MemoryOutcomeStore::new());
    let key = |provider: &str, model: &str| OutcomeKey {
        provider: provider.into(),
        model: model.into(),
        phase: RouterPhase::Implement,
        task_class: TaskClass::Medium,
        risk_bucket: RiskBucket::Low,
    };
    for _ in 0..20 {
        store.append_sample(
            &key("e1", "cheap"),
            OutcomeSample {
                verified_success: false,
                rework_cost_micro: 2_400_000,
                rework_turns: 3,
            },
        );
    }
    for _ in 0..100 {
        store.append_sample(
            &key("f1", "big"),
            OutcomeSample {
                verified_success: true,
                rework_cost_micro: 0,
                rework_turns: 0,
            },
        );
    }
    let req = priced_req(40_000, 6_000, 80);
    let before = RouterService::with_route_candidates(
        vec![cheap.clone(), strong.clone()],
        Arc::new(EmptyOutcomeStore),
    )
    .route(&req, &[])
    .unwrap();
    assert_eq!(
        before.provider, "e1",
        "without history the cheap quote wins"
    );
    let after = RouterService::with_route_candidates(vec![cheap, strong], store)
        .route(&req, &[])
        .unwrap();
    assert_eq!(
        after.provider, "f1",
        "verified outcomes must dominate the priors: {}",
        after.reasoning
    );
    assert!(after.reasoning.contains("verified"), "{}", after.reasoning);
    assert!(
        after.reasoning.contains("expected-cost"),
        "{}",
        after.reasoning
    );
}

// ==================================================================
// Hard-budget-safe pricing (audit): a Known quote past its validity
// window is no longer sufficient at route time; Stale exact is not a
// bound; a declared ceiling is the only hard-cap-eligible expiry path.
// ==================================================================

fn expired_exact_state() -> PricingState {
    PricingState::Known(
        PricingSnapshot::exact(exact_quote(10_000_000, 40_000_000), 7, "builtin-v2".into())
            .with_valid_until(1_000),
    )
}

#[test]
fn expired_exact_is_unknown_without_cap_and_inadmissible_under_hard_cap() {
    let expired = candidate(
        perf_desc("p", "expired", true, 256_000, 64_000, 90, 90),
        expired_exact_state(),
    );
    let now = 2_000u64; // past valid_until
    let uncapped = priced_req(1_000, 100, 50);
    let q = qualified_priced_candidates_at(
        std::slice::from_ref(&expired),
        &uncapped,
        &[],
        &LiveHealth::default(),
        now,
    )
    .unwrap();
    assert_eq!(
        q[0].cost,
        CostEstimate::Unknown,
        "an expired exact quote is never read as its old number"
    );
    // Without a cap the row is admitted as Unknown and the decision
    // freezes the Unknown snapshot (settlement refuses a fabricated
    // number) — never the stale exact price.
    let svc =
        RouterService::with_route_candidates(vec![expired.clone()], Arc::new(EmptyOutcomeStore));
    let d = svc.route_at(&uncapped, &[], now).unwrap();
    let snap = d.pricing_snapshot.expect("priced decision snapshots");
    assert_eq!(snap.authority, PriceAuthority::Unknown);
    assert_eq!(snap.settle_cost(1_000, 0, 0, 100), None);

    // Under a hard cap there is no honest bound: fail closed.
    let capped = RouteRequest {
        task_budget_remaining_micro: 10_000_000_000,
        ..priced_req(1_000, 100, 50)
    };
    let err = qualified_priced_candidates_at(
        std::slice::from_ref(&expired),
        &capped,
        &[],
        &LiveHealth::default(),
        now,
    )
    .expect_err("an expired exact quote cannot reserve under a hard cap");
    assert!(
        err.route_error().contains("budget/latency"),
        "{}",
        err.route_error()
    );
    // A fresh window keeps the same row exactly priced (control).
    let fresh = candidate(
        perf_desc("p", "expired", true, 256_000, 64_000, 90, 90),
        PricingState::Known(
            PricingSnapshot::exact(exact_quote(10_000_000, 40_000_000), 7, "x".into())
                .with_valid_until(3_000),
        ),
    );
    let q = qualified_priced_candidates_at(
        std::slice::from_ref(&fresh),
        &capped,
        &[],
        &LiveHealth::default(),
        now,
    )
    .unwrap();
    assert!(matches!(q[0].cost, CostEstimate::Known(_)));
}

#[test]
fn expired_exact_with_a_documented_ceiling_is_admissible_under_hard_cap() {
    let ceiling = exact_quote(50_000_000, 200_000_000);
    let bounded = candidate(
        perf_desc("p", "bounded", true, 256_000, 64_000, 90, 90),
        PricingState::Known(
            PricingSnapshot::exact(exact_quote(1_000_000, 4_000_000), 7, "builtin-v2".into())
                .with_valid_until(1_000)
                .with_conservative_ceiling(ceiling),
        ),
    );
    let usage = TokenUsage::new(1_000, 0, 0, 100);
    let expected = ceiling.quote_cost_micro(usage);
    let mut capped = priced_req(1_000, 100, 50);
    capped.task_budget_remaining_micro = expected + 1;
    let q = qualified_priced_candidates_at(
        std::slice::from_ref(&bounded),
        &capped,
        &[],
        &LiveHealth::default(),
        2_000,
    )
    .unwrap();
    assert_eq!(
        q[0].cost,
        CostEstimate::Conservative(expected),
        "the documented ceiling is what a hard cap reserves"
    );
    let svc =
        RouterService::with_route_candidates(vec![bounded.clone()], Arc::new(EmptyOutcomeStore));
    let d = svc.route_at(&capped, &[], 2_000).unwrap();
    let snap = d.pricing_snapshot.expect("snapshot");
    assert_eq!(snap.authority, PriceAuthority::ConservativeCeiling);
    assert_eq!(snap.settle_cost(1_000, 0, 0, 100), Some(expected));
    // Without a hard cap the SAME expired row is Unknown: a ceiling is
    // a hard-cap instrument only.
    let mut uncapped = capped.clone();
    uncapped.task_budget_remaining_micro = 0;
    let d = svc.route_at(&uncapped, &[], 2_000).unwrap();
    assert_eq!(
        d.pricing_snapshot.expect("snapshot").authority,
        PriceAuthority::Unknown
    );
}

#[test]
fn stale_exact_is_never_hard_cap_eligible() {
    let stale = candidate(
        perf_desc("p", "stale", true, 256_000, 64_000, 90, 90),
        PricingState::Stale {
            last_known: PricingSnapshot::exact(exact_quote(1_000_000, 4_000_000), 7, "aged".into()),
            observed_at_ms: 5,
        },
    );
    let effective = stale.effective_pricing(9_999, true);
    assert!(
        effective.is_unknown(),
        "a stale exact quote is not a conservative bound"
    );
    let mut capped = priced_req(1_000, 100, 50);
    capped.task_budget_remaining_micro = 10_000_000_000;
    assert!(
        qualified_priced_candidates_at(
            std::slice::from_ref(&stale),
            &capped,
            &[],
            &LiveHealth::default(),
            9_999,
        )
        .is_err(),
        "stale exact must fail closed under a hard cap"
    );
    // Without a cap it is admitted as Unknown, and the decision says so.
    let uncapped = priced_req(1_000, 100, 50);
    let svc = RouterService::with_route_candidates(vec![stale], Arc::new(EmptyOutcomeStore));
    let d = svc.route_at(&uncapped, &[], 9_999).unwrap();
    let snap = d.pricing_snapshot.expect("snapshot");
    assert_eq!(snap.authority, PriceAuthority::Unknown);
    assert_eq!(snap.settle_cost(1_000, 0, 0, 100), None);
}

#[test]
fn conservative_ceiling_state_stays_admissible_under_hard_cap() {
    let bounded = candidate(
        perf_desc("p", "bound", true, 256_000, 64_000, 90, 90),
        PricingState::ConservativeCeiling(PricingSnapshot::conservative_ceiling(
            exact_quote(2_000_000, 8_000_000),
            2,
            "user-v1".into(),
        )),
    );
    let mut capped = priced_req(1_000, 100, 50);
    let expected =
        exact_quote(2_000_000, 8_000_000).quote_cost_micro(TokenUsage::new(1_000, 0, 0, 100));
    capped.task_budget_remaining_micro = expected;
    let q = qualified_priced_candidates_at(
        std::slice::from_ref(&bounded),
        &capped,
        &[],
        &LiveHealth::default(),
        u64::MAX,
    )
    .unwrap();
    assert_eq!(q[0].cost, CostEstimate::Conservative(expected));
    let svc = RouterService::with_route_candidates(vec![bounded], Arc::new(EmptyOutcomeStore));
    let d = svc.route_at(&capped, &[], u64::MAX).unwrap();
    let snap = d.pricing_snapshot.expect("snapshot");
    assert_eq!(snap.authority, PriceAuthority::ConservativeCeiling);
    assert_eq!(snap.settle_cost(1_000, 0, 0, 100), Some(expected));
}

#[test]
fn tariff_dependent_cache_write_reserves_the_conservative_applicable_amount() {
    // An Anthropic-shaped snapshot reserves the MAX documented
    // cache-write tariff (1h = 30M/M) for a hard budget, so cache
    // creation can never be reserved at zero or at the cheaper TTL.
    let quote = PriceQuote {
        input: MicroUsdPerMillionTokens(15_000_000),
        output: MicroUsdPerMillionTokens(75_000_000),
        cache_read: MicroUsdPerMillionTokens(1_500_000),
        cache_write: MicroUsdPerMillionTokens(30_000_000),
    };
    let c = candidate(
        perf_desc("anthropic", "claude-opus-4", true, 256_000, 64_000, 90, 90),
        PricingState::Known(PricingSnapshot::exact(quote, 1, "builtin-v2".into())),
    );
    let cache = [CacheState {
        provider: "anthropic".into(),
        model: "claude-opus-4".into(),
        cached_input_tokens: 1_000,
        will_write_tokens: 2_000,
    }];
    let mut req = priced_req(10_000, 500, 50);
    req.task_budget_remaining_micro = 0;
    let estimate = CostEstimate::from_effective(
        &c.effective_pricing(unix_now_ms(), true),
        TokenUsage::new(9_000, 1_000, 2_000, 500),
    );
    let expected = quote.quote_cost_micro(TokenUsage::new(9_000, 1_000, 2_000, 500));
    assert_eq!(estimate, CostEstimate::Known(expected));
    assert!(expected >= 2_000 * 30_000_000 / 1_000_000);
    // The same usage fits a cap exactly at the reserved amount.
    req.task_budget_remaining_micro = expected;
    let q = qualified_priced_candidates_at(
        std::slice::from_ref(&c),
        &req,
        &cache,
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .unwrap();
    assert_eq!(q[0].cost.numeric(), Some(expected));
    // One micro below the conservative reservation fails the cap.
    req.task_budget_remaining_micro = expected - 1;
    assert!(qualified_priced_candidates_at(
        std::slice::from_ref(&c),
        &req,
        &cache,
        &LiveHealth::default(),
        unix_now_ms(),
    )
    .is_err());
}
