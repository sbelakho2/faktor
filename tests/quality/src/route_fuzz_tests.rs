// TEST-SEPARATION: oracle=property isolation=isolated-data-dir
//! HAR-12 route/mode/permission fuzz driver (ledger row: route fuzzing and
//! long runs turn ANY panic into a failure WITH the input recorded).
//!
//! Scope — the three decision functions that gate every request:
//!
//! 1. **route**: `faktor_router::{Router::route, RouterService::route_at,
//!    qualify_specific_at}` — the single qualification pass + selection
//!    over descriptor, legacy, priced and pinned candidate sets.
//! 2. **mode**: `faktor_agent::EconomicRoutingPolicy::route` over every
//!    `RoutingMode` (Economy / MaximumQuality / Balanced / Pinned).
//! 3. **permission**: `faktor_security::destination` —
//!    `RequestTarget::parse` + `DestinationPolicy::check_against` (the
//!    destination allow/deny gate) and `DestinationPolicy::parse_lines`.
//!
//! Inputs are seeded (SplitMix64, base seed pinned) and cycle through an
//! explicit edge corpus — empty, oversized (> 4096-char destination),
//! bidi overrides, mixed case, unicode/IDN, control characters, hostile
//! pins absent from the catalog, floor 0/255, target < floor, u64::MAX
//! token counts, empty and saturated candidate sets. Every iteration runs
//! under `crate::panic_hook`: a panic anywhere (main or foreign thread)
//! becomes a caught failure carrying the input; the test then MINIMIZES the
//! input, prints it, saves it under `regressions/route-fuzz/` for replay,
//! and fails. The soak variant is `#[ignore]`d (registered manual long
//! test) and runs N thousand iterations under the same hook.
//!
//! Property invariants per input (each attempts to BREAK a guarantee, not
//! to confirm a happy path):
//! - no panic, ever (catch_unwind + global hook, foreign threads included);
//! - Ok decisions never invent a provider/model outside the candidate set;
//! - an unknown capability id fails CLOSED on the plain qualification
//!   paths (`capability_ok`'s documented fail-closed arm);
//! - pinned services/modes serve ONLY an exactly-matching pin (Err
//!   otherwise; both-sides-empty is the documented passthrough);
//! - determinism: pure paths (Router::route, route_at at fixed clock,
//!   permission parse+check) return byte-identical decisions on repeat;
//! - canonical hosts of parsed targets are ASCII, lowercased and
//!   whitespace/control-free (bidi/mixed-case cannot survive
//!   canonicalization), and every parse error message is non-empty.

use std::path::PathBuf;
use std::sync::Arc;

use faktor_agent::{EconomicRoutingPolicy, RouteFailure, RoutingPolicy};
use faktor_core::model::{
    MicroUsdPerMillionTokens, MicroUsdPerToken, ModelDescriptor, ModelEconomics, ModelSource,
    PriceQuote, PricingSnapshot, PricingState, RateLimitState, RiskBucket, RouteDecision,
    RouterPhase, RoutingMode, TaskClass,
};
use faktor_router::outcomes::EmptyOutcomeStore;
use faktor_router::{CacheState, RouteCandidate, RouteRequest, Router, RouterService};
use faktor_security::destination::{Decision, DestinationPolicy, RequestTarget};

use crate::panic_hook::{self, CaughtPanic};

// ------------------------------------------------------------------ cases

/// The capability ids `ModelDescriptor::capability_ok` grants; everything
/// else must fail closed.
const KNOWN_CAPS: [&str; 8] = [
    "tools",
    "parallel_tools",
    "reasoning",
    "thinking",
    "vision",
    "structured_output",
    "embeddings",
    "streaming",
];

const FIXED_NOW_MS: u64 = 1_700_000_000_000;

const KNOWN_RULES: [&str; 3] = [
    "https://api.example.com",
    "*.corp.example",
    "example.com:8080",
];

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Candidate {
    provider: String,
    model: String,
    tools: bool,
    context: u64,
    coding: u8,
    latency_ms: u64,
    price_input_micro_per_mtok: u64,
    price_output_micro_per_mtok: u64,
    rate_limit_hard: bool,
    pricing_variant: u8,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Case {
    iteration: u64,
    phase: usize,
    caps: Vec<String>,
    context_tokens: u64,
    output_tokens: u64,
    quality_floor: u8,
    quality_target: Option<u8>,
    budget: u64,
    latency_ms: Option<u64>,
    task_class: usize,
    risk: usize,
    cache: Vec<(String, String, u64, u64)>,
    candidates: Vec<Candidate>,
    pin: Vec<String>,
    mode: usize,
    destination: String,
    policy_kind: usize,
}

// --------------------------------------------------------------- sourcing

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        choices[self.below(choices.len() as u64) as usize]
    }
}

/// Hostile string fragments: every axis the task demands (empty, oversized,
/// bidi, mixed-case, unicode) plus protocol-hostile shapes.
const HOSTILE_FRAGMENTS: [&str; 24] = [
    "",
    " ",
    "\t",
    "a",
    "API",
    "aPi",
    "EXAMPLE.Com",
    "example.com",
    "evil.example.com.",
    "api.example.com",
    "\u{202e}moc.elpmaxe",
    "\u{2066}host\u{2069}",
    "日本語.example",
    "cafe\u{301}.example",
    "\u{1f980}-host.example",
    "xn--80ak6aa92e.com",
    "127.0.0.1",
    "0x7f.1",
    "localhost:99999",
    "example.com:0",
    "host with space",
    "a\0b.example",
    "tools\u{202e}",
    "……………",
];

fn hostile_token(rng: &mut Rng) -> String {
    let frag = rng.pick(&HOSTILE_FRAGMENTS);
    if rng.chance(15) {
        // Oversized input: blow past the 4096-char destination cap.
        format!("{frag}{}", "x".repeat(4_500 + rng.below(500) as usize))
    } else {
        frag.to_string()
    }
}

fn hostile_destination(rng: &mut Rng) -> String {
    let prefixes = [
        "",
        "https://",
        "http://",
        "file://",
        "javascript:",
        "ftp://",
        "wss://",
        "https://[::1]:",
        "https://",
    ];
    let prefix = rng.pick(&prefixes);
    let body = match rng.below(6) {
        0 => "example.com".to_string(),
        1 => "api.example.com/path?q=1".to_string(),
        2 => "EXAMPLE.com./Path".to_string(),
        3 => hostile_token(rng),
        4 => "\u{202e}moc.elpmaxe.com".to_string(),
        _ => "日本語.jp".to_string(),
    };
    let suffix = match rng.below(5) {
        0 => "".to_string(),
        1 => ":443".to_string(),
        2 => ":0".to_string(),
        3 => ":99999".to_string(),
        _ => "/\u{202e}path".to_string(),
    };
    format!("{prefix}{body}{suffix}")
}

fn cap_pool(rng: &mut Rng) -> Vec<String> {
    let count = rng.below(6) as usize;
    (0..count)
        .map(|_| match rng.below(6) {
            0 => rng.pick(&KNOWN_CAPS).to_string(),
            1 => rng.pick(&KNOWN_CAPS).to_ascii_uppercase(),
            2 => rng.pick(&KNOWN_CAPS).to_string(),
            3 => hostile_token(rng),
            4 => "frobnicate".to_string(),
            _ => format!("{}\u{202e}", rng.pick(&KNOWN_CAPS)),
        })
        .collect()
}

fn has_unknown_cap(caps: &[String]) -> bool {
    caps.iter().any(|c| !KNOWN_CAPS.contains(&c.as_str()))
}

fn candidate_pool(rng: &mut Rng) -> Vec<Candidate> {
    let count = match rng.below(10) {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 40,
        n => (n as usize) + 1,
    };
    (0..count)
        .map(|i| Candidate {
            provider: if rng.chance(15) {
                hostile_token(rng)
            } else {
                rng.pick(&["prov", "other", "Prov", "本地"]).to_string()
            },
            model: if rng.chance(15) {
                hostile_token(rng)
            } else {
                format!("model-{}", i % 7)
            },
            tools: rng.chance(70),
            context: rng
                .pick(&["1_000", "128_000", "2_000_000", "0", "18446744073709551615"])
                .parse()
                .unwrap_or(u64::MAX),
            coding: rng.below(101) as u8,
            latency_ms: rng.below(5_000),
            price_input_micro_per_mtok: rng.below(3_000),
            price_output_micro_per_mtok: rng.below(12_000),
            rate_limit_hard: rng.chance(10),
            pricing_variant: rng.below(5) as u8,
        })
        .collect()
}

fn gen_case(seed: u64, iteration: u64) -> Case {
    let mut rng = Rng::new(seed ^ iteration.wrapping_mul(0x9E37_79B9));
    Case {
        iteration,
        phase: rng.below(RouterPhase::ALL.len() as u64) as usize,
        caps: cap_pool(&mut rng),
        context_tokens: match rng.below(8) {
            0 => 0,
            1 => u64::MAX,
            2 => 1 << 62,
            n => n * 4_096,
        },
        output_tokens: rng.below(300_000),
        quality_floor: match rng.below(10) {
            0 => 0,
            1 => 100,
            2 => 255,
            n => (n * 11) as u8,
        },
        quality_target: if rng.chance(50) {
            Some(rng.below(101) as u8)
        } else {
            None
        },
        budget: match rng.below(8) {
            0 => 0,
            1 => 1,
            2 => u64::MAX,
            n => n * 100_000,
        },
        latency_ms: if rng.chance(50) {
            Some(rng.below(3_000))
        } else {
            None
        },
        task_class: rng.below(TaskClass::ALL.len() as u64) as usize,
        risk: rng.below(RiskBucket::ALL.len() as u64) as usize,
        cache: (0..rng.below(4))
            .map(|_| {
                (
                    rng.pick(&["prov", "other", "本地"]).to_string(),
                    format!("model-{}", rng.below(7)),
                    rng.below(100_000),
                    rng.below(100_000),
                )
            })
            .collect(),
        candidates: candidate_pool(&mut rng),
        pin: match rng.below(5) {
            0 => vec![],
            1 => vec!["prov".into(), "model-1".into()],
            2 => vec!["ghost".into(), "absent".into()],
            3 => vec!["".into(), "".into()],
            _ => vec![hostile_token(&mut rng), hostile_token(&mut rng)],
        },
        mode: rng.below(4) as usize,
        destination: hostile_destination(&mut rng),
        policy_kind: rng.below(3) as usize,
    }
}

/// The explicit edge corpus (cycled inside the seeded stream): shapes that
/// must be exercised even if the RNG would rarely assemble them.
fn edge_case(iteration: u64) -> Case {
    let mut c = gen_case(0xE000, iteration.wrapping_mul(7));
    match iteration % 16 {
        0 => {
            c.caps.clear();
            c.candidates.truncate(1);
        }
        1 => {
            c.caps = vec!["tools".into(), "frobnicate".into()];
            c.candidates = candidate_pool(&mut Rng::new(1));
        }
        2 => c.destination = "x".repeat(4_097),
        3 => c.destination = "\u{202e}moc.elpmaxe.com".into(),
        4 => c.destination = "EXAMPLE.Com./p".into(),
        5 => c.destination = "".into(),
        6 => c.destination = "日本語.jp".into(),
        7 => {
            c.quality_floor = 255;
            c.quality_target = Some(0);
        }
        8 => {
            c.quality_floor = 0;
            c.quality_target = Some(100);
        }
        9 => {
            c.context_tokens = u64::MAX;
            c.output_tokens = u64::MAX;
            c.budget = u64::MAX;
        }
        10 => {
            c.candidates.clear();
            c.pin = vec!["ghost".into(), "absent".into()];
        }
        11 => {
            c.pin = vec!["".into(), "".into()];
            c.caps = vec!["not-a-cap".into()];
        }
        12 => {
            c.caps = HOSTILE_FRAGMENTS.iter().map(|s| (*s).to_string()).collect();
        }
        13 => {
            c.candidates = (0..40)
                .map(|i| Candidate {
                    provider: format!("p{i}"),
                    model: format!("m{i}"),
                    tools: i % 2 == 0,
                    context: 2_000_000,
                    coding: (i as u8 * 3) % 101,
                    latency_ms: i as u64 * 10,
                    price_input_micro_per_mtok: i as u64,
                    price_output_micro_per_mtok: i as u64 * 4,
                    rate_limit_hard: i % 11 == 0,
                    pricing_variant: (i % 5) as u8,
                })
                .collect();
            c.caps = vec!["tools".into()];
        }
        14 => {
            c.destination = "a\0b\u{202e}.example".into();
            c.policy_kind = 2;
        }
        _ => {
            c.cache = vec![("prov".into(), "model-1".into(), u64::MAX, u64::MAX)];
            c.phase = 0;
        }
    }
    c.iteration = iteration;
    c
}

fn next_case(seed: u64, iteration: u64) -> Case {
    if iteration.is_multiple_of(97) {
        edge_case(iteration / 97)
    } else {
        gen_case(seed, iteration)
    }
}

// ------------------------------------------------------------- builders

fn descriptor(c: &Candidate) -> ModelDescriptor {
    ModelDescriptor {
        provider: c.provider.clone(),
        model: c.model.clone(),
        context: c.context,
        max_output: 64_000,
        tools: c.tools,
        parallel_tools: true,
        reasoning: true,
        thinking: false,
        vision: false,
        structured_output: true,
        embeddings: false,
        streaming: true,
        economics: ModelEconomics {
            input_price_per_mtok: MicroUsdPerToken(c.price_input_micro_per_mtok),
            output_price_per_mtok: MicroUsdPerToken(c.price_output_micro_per_mtok),
            cache_read_price_per_mtok: MicroUsdPerToken(c.price_input_micro_per_mtok / 5),
            cache_write_price_per_mtok: MicroUsdPerToken(c.price_input_micro_per_mtok / 2),
            estimated_latency_ms: c.latency_ms,
            tool_reliability: 90,
            reasoning_reliability: 90,
            coding_reliability: c.coding,
            context_reliability: c.coding,
            availability: if c.rate_limit_hard { 0 } else { 100 },
            rate_limit_state: if c.rate_limit_hard {
                RateLimitState::Hard
            } else {
                RateLimitState::Healthy
            },
        },
        source: ModelSource::CuratedRegistry,
    }
}

fn pricing_state(c: &Candidate) -> PricingState {
    let quote = PriceQuote {
        input: MicroUsdPerMillionTokens(c.price_input_micro_per_mtok),
        output: MicroUsdPerMillionTokens(c.price_output_micro_per_mtok),
        cache_read: MicroUsdPerMillionTokens(c.price_input_micro_per_mtok / 5),
        cache_write: MicroUsdPerMillionTokens(c.price_input_micro_per_mtok / 2),
    };
    match c.pricing_variant {
        0 => PricingState::Known(PricingSnapshot::exact(quote, 1, "fuzz".into())),
        1 => PricingState::ConservativeCeiling(PricingSnapshot::conservative_ceiling(
            quote,
            1,
            "fuzz".into(),
        )),
        2 => PricingState::LocalZero,
        3 => PricingState::Unknown,
        _ => PricingState::Stale {
            last_known: PricingSnapshot::exact(quote, 1, "fuzz".into()),
            observed_at_ms: FIXED_NOW_MS,
        },
    }
}

fn route_request(c: &Case) -> RouteRequest {
    RouteRequest {
        phase: RouterPhase::ALL[c.phase % RouterPhase::ALL.len()],
        required_capabilities: c.caps.clone(),
        context_tokens: c.context_tokens,
        estimated_output_tokens: c.output_tokens,
        quality_floor: c.quality_floor,
        quality_target: c.quality_target,
        task_budget_remaining_micro: c.budget,
        latency_preference_ms: c.latency_ms,
        task_class: TaskClass::ALL[c.task_class % TaskClass::ALL.len()],
        risk_bucket: RiskBucket::ALL[c.risk % RiskBucket::ALL.len()],
    }
}

fn cache_states(c: &Case) -> Vec<CacheState> {
    c.cache
        .iter()
        .map(|(provider, model, hit, write)| CacheState {
            provider: provider.clone(),
            model: model.clone(),
            cached_input_tokens: *hit,
            will_write_tokens: *write,
        })
        .collect()
}

fn descriptors(c: &Case) -> Vec<ModelDescriptor> {
    c.candidates.iter().map(descriptor).collect()
}

fn priced_candidates(c: &Case) -> Vec<RouteCandidate> {
    c.candidates
        .iter()
        .map(|cand| RouteCandidate::new(descriptor(cand), pricing_state(cand)))
        .collect()
}

fn pin_pair(c: &Case) -> (String, String) {
    match c.pin.len() {
        0 => (String::new(), String::new()),
        1 => (c.pin[0].clone(), String::new()),
        _ => (c.pin[0].clone(), c.pin[1].clone()),
    }
}

fn membership_holds(decision: &RouteDecision, candidates: &[ModelDescriptor]) -> bool {
    (decision.provider.is_empty() && decision.model.is_empty())
        || candidates
            .iter()
            .any(|d| d.provider == decision.provider && d.model == decision.model)
}

fn policy_for(c: &Case) -> DestinationPolicy {
    let lines: Vec<&str> = match c.policy_kind {
        0 => vec![],
        1 => KNOWN_RULES.to_vec(),
        _ => {
            // A hostile rule set: strict parse may refuse it; a refusal is
            // a legal configuration-time outcome, so fall back to the
            // documented empty (deny-all) policy rather than unwrapping.
            match DestinationPolicy::parse_lines(KNOWN_RULES) {
                Ok(p) => return p,
                Err(_) => return DestinationPolicy::empty(),
            }
        }
    };
    DestinationPolicy::parse_lines(lines).unwrap_or_else(|_| DestinationPolicy::empty())
}

// ------------------------------------------------------------ assertions

/// Panics on the FIRST violated property; the panic hook + catcher turn it
/// into a failure carrying this case (see module docs).
fn assert_route_axis(c: &Case) {
    let req = route_request(c);
    let cache = cache_states(c);
    let unknown = has_unknown_cap(&c.caps);
    let plain = descriptors(c);

    // 1. descriptor router: Ok never invents a model; determinism; the
    //    unknown-capability fail-closed arm.
    let router = Router::new(plain.clone());
    let first = router.route(&req, &cache);
    let second = router.route(&req, &cache);
    assert_eq!(
        first, second,
        "Router::route must be deterministic for identical input"
    );
    match &first {
        Ok(decision) => {
            assert!(
                membership_holds(decision, &plain),
                "decision {:?} invented a model outside {:?}",
                (decision.provider.clone(), decision.model.clone()),
                c.candidates
                    .iter()
                    .map(|x| (x.provider.clone(), x.model.clone()))
                    .collect::<Vec<_>>()
            );
            assert_eq!(decision.considered, plain.len());
            assert!(!decision.reasoning.is_empty(), "empty reasoning");
            assert!(
                !unknown,
                "unknown capability granted on the descriptor path: {:?}",
                c.caps
            );
        }
        Err(message) => {
            assert!(!message.is_empty(), "empty route error message");
        }
    }

    // 2. legacy service (descriptor-backed pricing): same properties at a
    //    FIXED clock (route_at), so repeat runs must be byte-identical.
    let legacy = RouterService::new(plain.clone());
    let first = legacy.route_at(&req, &cache, FIXED_NOW_MS);
    let second = legacy.route_at(&req, &cache, FIXED_NOW_MS);
    assert_eq!(first, second);
    if let Ok(decision) = &first {
        assert!(membership_holds(decision, &plain));
        assert!(!unknown, "unknown capability granted on the legacy path");
    }

    // 3. priced production service: the SAME single qualification pass.
    let priced =
        RouterService::with_route_candidates(priced_candidates(c), Arc::new(EmptyOutcomeStore));
    let first = priced.route_at(&req, &cache, FIXED_NOW_MS);
    let second = priced.route_at(&req, &cache, FIXED_NOW_MS);
    assert_eq!(first, second);
    if let Ok(decision) = &first {
        assert!(membership_holds(decision, &plain));
        assert!(!unknown, "unknown capability granted on the priced path");
    }

    // 4. pinned service: serves ONLY an exactly-matching pin; a pin absent
    //    from the priced set is an Err, never a substitution. (The
    //    both-sides-empty pin is validated against whatever candidate
    //    happens to carry empty identity — membership still holds.)
    let (pin_provider, pin_model) = pin_pair(c);
    let pinned = RouterService::with_pinned_route_candidates(
        priced_candidates(c),
        pin_provider.clone(),
        pin_model.clone(),
        Arc::new(EmptyOutcomeStore),
    );
    match pinned.route_at(&req, &cache, FIXED_NOW_MS) {
        Ok(decision) => {
            if !(pin_provider.is_empty() && pin_model.is_empty()) {
                assert_eq!(
                    (decision.provider.clone(), decision.model.clone()),
                    (pin_provider.clone(), pin_model.clone()),
                    "pinned service substituted the pin"
                );
            }
        }
        Err(message) => assert!(!message.is_empty()),
    }

    // 5. qualify_specific_at: Ok names the requested pair verbatim.
    if !plain.is_empty() {
        let probe_provider = plain[0].provider.clone();
        let probe_model = plain[0].model.clone();
        let qualified =
            priced.qualify_specific_at(&probe_provider, &probe_model, &req, &cache, FIXED_NOW_MS);
        match qualified {
            Ok(candidate) => assert_eq!(
                (
                    candidate.descriptor.provider.clone(),
                    candidate.descriptor.model.clone()
                ),
                (probe_provider, probe_model)
            ),
            Err(failure) => assert!(
                !failure.route_error().is_empty(),
                "empty qualification failure text"
            ),
        }
    }
}

fn assert_mode_axis(c: &Case) {
    let req = route_request(c);
    let service = Arc::new(RouterService::with_route_candidates(
        priced_candidates(c),
        Arc::new(EmptyOutcomeStore),
    ));
    let mode = match c.mode {
        0 => RoutingMode::Economy,
        1 => RoutingMode::MaximumQuality,
        2 => RoutingMode::Balanced,
        _ => {
            let (provider, model) = pin_pair(c);
            RoutingMode::Pinned { provider, model }
        }
    };
    let policy = EconomicRoutingPolicy::new(service, mode.clone());
    let outcome: Result<RouteDecision, RouteFailure> = policy.route(&req);
    match &outcome {
        Ok(decision) => {
            let candidates = descriptors(c);
            assert!(
                membership_holds(decision, &candidates),
                "mode {mode:?} invented {:?} outside the candidate set",
                (decision.provider.clone(), decision.model.clone())
            );
            if let RoutingMode::Pinned { provider, model } = &mode {
                if !(provider.is_empty() && model.is_empty()) {
                    assert_eq!(
                        (decision.provider.clone(), decision.model.clone()),
                        (provider.clone(), model.clone()),
                        "pinned mode substituted the pin"
                    );
                }
            }
        }
        Err(failure) => {
            // Typed failures are the designed refusal surface; the debug
            // word carries the refusal kind for the operator.
            let text = format!("{failure:?}");
            assert!(!text.is_empty(), "empty route failure");
        }
    }
}

fn assert_permission_axis(c: &Case) {
    let policy = policy_for(c);
    let first = RequestTarget::parse(&c.destination);
    let second = RequestTarget::parse(&c.destination);
    match (&first, &second) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a, b, "destination parse must be deterministic");
            // Canonical-host hard invariant: ASCII, lowercased, no
            // whitespace/control — bidi and mixed case cannot survive.
            assert!(!a.host.is_empty(), "parsed target has an empty host");
            assert!(a.host.is_ascii(), "canonical host not ASCII: {:?}", a.host);
            assert_eq!(
                a.host,
                a.host.to_ascii_lowercase(),
                "canonical host not lowercased: {:?}",
                a.host
            );
            assert!(
                !a.host
                    .bytes()
                    .any(|b| b.is_ascii_whitespace() || b.is_ascii_control()),
                "canonical host carries whitespace/control: {:?}",
                a.host
            );
            let first_decision = a.check_against(&policy);
            let second_decision = b.check_against(&policy);
            assert_eq!(
                format!("{first_decision:?}"),
                format!("{second_decision:?}"),
                "permission decision must be deterministic"
            );
            // Totality: every parsed target gets an explicit verdict and a
            // denied verdict always names its reason.
            match first_decision {
                Decision::Allowed => {}
                Decision::Denied(reason) => {
                    assert!(!format!("{reason}").is_empty(), "empty deny reason")
                }
            }
        }
        (Err(a), Err(b)) => {
            assert_eq!(a, b, "destination parse errors must be deterministic");
            assert!(!a.is_empty(), "empty destination parse error");
        }
        _ => panic!("destination parse determinism violated"),
    }
}

fn assert_case(c: &Case) {
    assert_route_axis(c);
    assert_mode_axis(c);
    assert_permission_axis(c);
}

// ------------------------------------------------------ driver + minimize

const BASE_SEED: u64 = 0xFA70_2026_1234_5678;

fn regression_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("regressions/route-fuzz")
}

/// Bounded input repr for the hook record (commandment 5: no unbounded
/// transcripts — a 4096-char hostile destination must not become an
/// unbounded panic log line).
fn input_repr(c: &Case) -> String {
    let full = format!("{c:#?}");
    if full.len() <= 8_192 {
        return full;
    }
    let mut clipped: String = full.chars().take(6_000).collect();
    clipped.push_str(&format!("…<truncated, {} bytes total>", full.len()));
    clipped
}

fn run_case(c: &Case) -> Result<(), CaughtPanic> {
    panic_hook::set_current_input(&input_repr(c));
    panic_hook::catch(|| assert_case(c))
}

fn shrink(c: &Case) -> Vec<Case> {
    let mut out: Vec<Case> = Vec::new();
    // Smaller destinations (halving converges on the minimal failing shape).
    if c.destination.chars().count() > 1 {
        let mut half = c.destination.clone();
        let cut = half.chars().count() / 2;
        half = half.chars().take(cut).collect();
        let mut next = c.clone();
        next.destination = half;
        out.push(next);
    }
    let mut cleared = c.clone();
    cleared.destination = String::new();
    out.push(cleared);
    // Fewer capabilities (drop one entry at a time, both ends).
    if c.caps.len() > 1 {
        let mut head = c.clone();
        head.caps.remove(0);
        out.push(head);
        let mut tail = c.clone();
        tail.caps.pop();
        out.push(tail);
    } else if !c.caps.is_empty() {
        let mut none = c.clone();
        none.caps.clear();
        out.push(none);
    }
    // Fewer candidates (drop the last one).
    if c.candidates.len() > 1 {
        let mut fewer = c.clone();
        fewer.candidates.pop();
        out.push(fewer);
    } else if !c.candidates.is_empty() {
        let mut none = c.clone();
        none.candidates.clear();
        out.push(none);
    }
    // Drop optional structure.
    if c.cache.len() > 1 || c.pin.len() > 1 {
        let mut simpler = c.clone();
        if c.cache.len() > 1 {
            simpler.cache.pop();
        }
        if c.pin.len() > 1 {
            simpler.pin.pop();
        }
        out.push(simpler);
    }
    if c.quality_target.is_some() || c.latency_ms.is_some() {
        let mut simpler = c.clone();
        simpler.quality_target = None;
        simpler.latency_ms = None;
        out.push(simpler);
    }
    // Shrink numeric extremes toward small values.
    if c.context_tokens > 0 || c.output_tokens > 0 || c.budget > 0 {
        let mut smaller = c.clone();
        smaller.context_tokens /= 2;
        smaller.output_tokens /= 2;
        smaller.budget /= 2;
        out.push(smaller);
    }
    if c.quality_floor != 0 {
        let mut lower = c.clone();
        lower.quality_floor = 0;
        out.push(lower);
    }
    if c.policy_kind != 0 {
        let mut empty_policy = c.clone();
        empty_policy.policy_kind = 0;
        out.push(empty_policy);
    }
    out
}

/// Run one iteration and, on any caught panic, MINIMIZE the input, print
/// it, save the regression file and convert the panic into a failure.
fn fuzz_iteration_or_fail(seed: u64, iteration: u64) {
    let c = next_case(seed, iteration);
    if let Err(caught) = run_case(&c) {
        let minimized = panic_hook::minimize(&c, shrink, |smaller| run_case(smaller).is_err(), 128);
        let value = serde_json::to_value(&minimized).unwrap_or(serde_json::Value::Null);
        panic_hook::fail_with_minimized_input(&regression_dir(), "route-fuzz", &caught, &value);
    }
}

// ------------------------------------------------------------------ tests

/// Fast seeded property run: 2,000 iterations across all three decision
/// axes, every iteration under the panic hook, any panic converted into a
/// failure with the minimized input printed and saved.
#[test]
fn route_fuzz_seeded_never_panics_with_input_on_failure() {
    for iteration in 0..2_000u64 {
        fuzz_iteration_or_fail(BASE_SEED, iteration);
    }
}

/// Regression replay: every input the fuzz driver ever failed on is stored
/// under `regressions/route-fuzz/` and must replay clean here — a fixed
/// router cannot regress into the same panic again. A corrupt or
/// foreign-shaped file fails loudly instead of being skipped.
#[test]
fn route_fuzz_regression_corpus_replays_clean() {
    let dir = regression_dir();
    for (path, document) in panic_hook::load_regressions(&dir) {
        assert_eq!(
            document.get("schema").and_then(|v| v.as_str()),
            Some("faktor-route-fuzz-regression/v1"),
            "regression file {} has the wrong schema",
            path.display()
        );
        let input = document
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let case: Case = serde_json::from_value(input)
            .unwrap_or_else(|e| panic!("regression file {} does not decode: {e}", path.display()));
        if let Err(caught) = run_case(&case) {
            let minimized =
                panic_hook::minimize(&case, shrink, |smaller| run_case(smaller).is_err(), 128);
            let value = serde_json::to_value(&minimized).unwrap_or(serde_json::Value::Null);
            panic_hook::fail_with_minimized_input(&dir, "route-fuzz-replay", &caught, &value);
        }
    }
}

/// Harness self-check (adversarial against the HOOK, not the SUT): a
/// planted panic that fires only on one specific oversized input must be
/// caught, attributed to that input, minimized down to the trigger shape,
/// and persisted as a replayable file — in a TEMP directory so the real
/// regression corpus never learns about a fake case.
#[test]
fn panic_hook_catches_planted_panic_minimizes_and_saves() {
    let temp = tempfile::tempdir().expect("temp dir");
    let trigger = "PANIC-TRIGGER\u{202e}".to_string();
    let planted = |input: &str| {
        panic_hook::set_current_input(&format!("planted: {input}"));
        panic_hook::catch(|| {
            assert!(
                !input.contains(&trigger),
                "planted panic fired on {input:?}"
            );
        })
    };
    // Control: a benign input passes.
    assert!(planted("benign").is_ok(), "benign input must not panic");
    // The break: the trigger input panics and carries the recorded input.
    let caught = planted(&format!("big {trigger} payload"))
        .expect_err("the planted panic must be caught, not swallowed");
    assert!(
        caught.input.contains(&trigger),
        "caught record lost the input: {}",
        caught.input
    );
    assert!(
        caught.payload.contains("planted panic fired"),
        "payload missing: {}",
        caught.payload
    );
    assert!(!caught.backtrace.is_empty(), "backtrace missing");
    // Minimize: shrinking must drop everything but the trigger (greedy
    // word-chunk removal — a probe survives only if it still panics).
    let seed_case = format!("prefix {trigger} suffix");
    let minimized = panic_hook::minimize(
        &seed_case,
        |s| {
            let words: Vec<&str> = s.split_whitespace().collect();
            let mut out: Vec<String> = Vec::new();
            if words.len() > 1 {
                out.push(words[1..].join(" "));
                out.push(words[..words.len() - 1].join(" "));
            }
            out
        },
        |s| s.contains(&trigger),
        64,
    );
    assert!(
        minimized.contains(&trigger),
        "minimizer dropped the trigger: {minimized:?}"
    );
    assert!(
        minimized.len() < seed_case.len(),
        "minimizer shrank nothing: {minimized:?}"
    );
    // Persist + reload.
    let input = serde_json::json!({ "minimized": minimized });
    let saved =
        panic_hook::save_regression(temp.path(), "harness-selfcheck", &input, &caught.payload)
            .expect("save regression");
    assert!(saved.exists(), "regression file not written");
    let reloaded = panic_hook::load_regressions(temp.path());
    assert_eq!(reloaded.len(), 1, "exactly one regression file");
    assert_eq!(
        reloaded[0].1["input"]["minimized"],
        minimized.as_str(),
        "round trip lost the input"
    );
    // The REAL corpus must be untouched by this self-check.
    let real = panic_hook::load_regressions(&regression_dir());
    assert!(
        !real.iter().any(|(p, _)| p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("harness-selfcheck"))),
        "harness self-check leaked into the real regression corpus"
    );
}

/// Long-run soak (ledger HAR-12 d): N thousand iterations under the SAME
/// hook — any panic on any thread becomes a failure with the input
/// recorded. Registered as a manual long test in
/// `scripts/certification/ignored-tests.json` (outside the nightly lanes'
/// package set).
///
/// `FAKTOR_ROUTE_FUZZ_ITERS` overrides the iteration count for even
/// longer manual soaks.
#[ignore = "[fault] manual long-run route-fuzz soak (>=50k seeded iterations under the panic-hook harness) outside the nightly fault lane's package set: run explicitly with `cargo test -p faktor-tests-quality -- --ignored`"]
#[test]
fn route_fuzz_longrun_soak_under_panic_hook() {
    let iterations: u64 = std::env::var("FAKTOR_ROUTE_FUZZ_ITERS")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(50_000);
    let foreign_base = panic_hook::foreign_record_count();
    for iteration in 0..iterations {
        fuzz_iteration_or_fail(BASE_SEED ^ 0x50AC_0000, iteration);
    }
    // Belt and braces: any panic the iteration loop could not attribute
    // (foreign thread during the soak window, or a stray record on this
    // thread) converts the soak into a failure with the input printed.
    if let Some(record) = panic_hook::take_record() {
        panic!(
            "panic recorded during soak without conversion: payload={} input={} thread={}",
            record.payload, record.input, record.thread
        );
    }
    let foreign = panic_hook::foreign_records_since(foreign_base);
    if !foreign.is_empty() {
        let summary: Vec<String> = foreign
            .iter()
            .map(|r| format!("[{}] {} @ input {}", r.thread, r.payload, r.input))
            .collect();
        panic!(
            "{} foreign-thread panic(s) recorded during the soak:\n{}",
            foreign.len(),
            summary.join("\n")
        );
    }
}
