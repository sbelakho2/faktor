//! The labeled FIXTURE capture generator.
//!
//! This produces `frozen/fixture/capture.json` — a `capture_source = fixture`
//! artifact driven by the deterministic, policy-BLIND scripted plan in
//! [`crate::coding::fixture_plan`]. Its only purpose is to prove the replay
//! and measurement machinery deterministically in CI:
//!
//! - it exercises first-pass, rework, local→frontier fallback, escalation,
//!   unsolvable runs, partial fixes and prompt-token accounting;
//! - it encodes NO model behavior (the scripted model ignores the prompt), so
//!   the fixture MUST NOT be published as model-quality or
//!   coding-competitiveness evidence;
//! - real model evidence comes exclusively from `capture_source = live`
//!   artifacts produced by the opt-in live-paid capture lane.
//!
//! The artifact is regenerated bit-for-bit by the benchmark test (a frozen
//! drift gate); changing the plan, corpus or pipeline without regenerating
//! the fixture fails the suite.

use std::path::Path;
use std::sync::Arc;

use faktor_core::model::{
    MicroUsdPerMillionTokens, ModelDescriptor, ModelEconomics, ModelSource, PriceQuote,
    PricingSnapshot, PricingState,
};

use crate::coding::{fixture_candidate, fixture_plan, load_coding_corpus, CodingTaskSpec};
use crate::frozen::{
    Arm, CaptureSource, CostSource, FrozenCapture, FrozenModel, TokenBasis, WallTimeBasis,
};
use crate::pipeline::{run_arm, CodingModel, CompletionRequest, ModelCompletion};

/// Deterministic timestamp base of fixture records (2023-11-14T22:13:20Z).
pub const FIXTURE_EPOCH_MS: i64 = 1_700_000_000_000;
/// Arithmetic local scripted model id (clearly not a real model).
pub const FIXTURE_LOCAL_MODEL: &str = "scripted-local-plumbing";
/// Arithmetic frontier scripted model id (clearly not a real model).
pub const FIXTURE_FRONTIER_MODEL: &str = "scripted-frontier-plumbing";
/// The fixture price table (microUSD per million tokens).
pub const FIXTURE_FRONTIER_INPUT_MICRO: u64 = 3_000_000;
pub const FIXTURE_FRONTIER_OUTPUT_MICRO: u64 = 12_000_000;

/// The scripted, policy-blind fixture model: it returns the plan's candidate
/// for (task, attempt) regardless of the prompt it received.
pub struct FixtureModel {
    routed: FrozenModel,
}

impl FixtureModel {
    fn new(routed: FrozenModel) -> Self {
        FixtureModel { routed }
    }
}

impl CodingModel for FixtureModel {
    fn routed(&self) -> &FrozenModel {
        &self.routed
    }

    fn complete(&self, request: &CompletionRequest<'_>) -> Result<ModelCompletion, String> {
        let plan = fixture_plan(request.task);
        let index = (request.attempt as usize).min(plan.len() - 1);
        let candidate = fixture_candidate(request.task, plan[index]);
        let text = candidate.to_json_text();
        let prompt = request.prompt();
        let tokens_in = prompt.tokens_est();
        let tokens_out = crate::pipeline::estimate_tokens(&text);
        let spend_micro = if self.routed.local {
            0
        } else {
            self.routed
                .pricing
                .snapshot()
                .settle_cost(tokens_in, 0, 0, tokens_out)
                .ok_or_else(|| "fixture frontier pricing cannot settle".to_string())?
        };
        Ok(ModelCompletion {
            text,
            tokens_in,
            tokens_out,
            spend_micro,
            cost_source: if self.routed.local {
                CostSource::LocalZero
            } else {
                CostSource::Fixture
            },
            latency_ms: self.routed.descriptor.economics.estimated_latency_ms,
        })
    }
}

fn fixture_descriptor(provider: &str, model: &str, local: bool) -> ModelDescriptor {
    let economics = if local {
        ModelEconomics {
            estimated_latency_ms: 250,
            tool_reliability: 80,
            reasoning_reliability: 76,
            coding_reliability: 82,
            context_reliability: 88,
            availability: 100,
            ..Default::default()
        }
    } else {
        ModelEconomics {
            estimated_latency_ms: 500,
            tool_reliability: 95,
            reasoning_reliability: 95,
            coding_reliability: 96,
            context_reliability: 96,
            availability: 100,
            ..Default::default()
        }
    };
    ModelDescriptor {
        provider: provider.into(),
        model: model.into(),
        context: 32_768,
        max_output: 4_096,
        tools: true,
        parallel_tools: false,
        reasoning: false,
        thinking: false,
        vision: false,
        structured_output: true,
        embeddings: false,
        streaming: true,
        economics,
        source: ModelSource::ConservativeDefault,
    }
}

/// The two frozen fixture models (local zero-cost + priced frontier).
pub fn fixture_models() -> Vec<FrozenModel> {
    vec![
        FrozenModel {
            descriptor: fixture_descriptor("fixture-local", FIXTURE_LOCAL_MODEL, true),
            pricing: PricingState::LocalZero,
            local: true,
            price_source: "fixture:local_zero".into(),
        },
        FrozenModel {
            descriptor: fixture_descriptor("fixture-frontier", FIXTURE_FRONTIER_MODEL, false),
            pricing: PricingState::Known(PricingSnapshot::exact(
                PriceQuote {
                    input: MicroUsdPerMillionTokens(FIXTURE_FRONTIER_INPUT_MICRO),
                    output: MicroUsdPerMillionTokens(FIXTURE_FRONTIER_OUTPUT_MICRO),
                    cache_read: MicroUsdPerMillionTokens(FIXTURE_FRONTIER_INPUT_MICRO / 4),
                    cache_write: MicroUsdPerMillionTokens(FIXTURE_FRONTIER_INPUT_MICRO / 2),
                },
                1,
                "fixture-price-table".into(),
            )),
            local: false,
            price_source: "fixture:price_table".into(),
        },
    ]
}

/// Generate the fixture capture deterministically.
pub fn generate_fixture_capture(scratch: &Path) -> Result<FrozenCapture, String> {
    let corpus = load_coding_corpus(&crate::coding::coding_corpus_root())?;
    let table = fixture_models();
    let models: Vec<Arc<dyn CodingModel>> = table
        .iter()
        .cloned()
        .map(|m| Arc::new(FixtureModel::new(m)) as Arc<dyn CodingModel>)
        .collect();
    let mut runs = Vec::with_capacity(corpus.tasks.len() * Arm::ALL.len());
    for arm in Arm::ALL {
        let arm_offset = if arm == Arm::FaktorMachinery { 1000 } else { 0 };
        let mut arm_runs = run_arm(
            &corpus,
            arm,
            &models,
            scratch,
            CaptureSource::Fixture,
            &move |task: &CodingTaskSpec| {
                FIXTURE_EPOCH_MS + i64::from(task.order) * 60_000 + arm_offset
            },
        )?;
        runs.append(&mut arm_runs);
    }
    FrozenCapture {
        schema: crate::frozen::FROZEN_SCHEMA.to_string(),
        capture_id: "fixture".into(),
        source: CaptureSource::Fixture,
        engine: format!(
            "faktor-tests-router-economics {}",
            env!("CARGO_PKG_VERSION")
        ),
        created_label: "deterministic labeled fixture (machinery proof only)".into(),
        corpus_digest: corpus.digest.clone(),
        token_basis: TokenBasis::Estimate,
        wall_time_basis: WallTimeBasis::Fixture,
        models: table,
        runs,
        digest: String::new(),
    }
    .finish()
}
