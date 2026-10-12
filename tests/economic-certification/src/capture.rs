//! Live capture lane (opt-in, paid): run REAL local (native Ollama) and
//! frontier (OpenAI-compatible) models through the shared coding-benchmark
//! pipeline, freeze their candidate outputs and final workspace states as a
//! `faktor-coding-benchmark-capture/v1` artifact, then immediately replay
//! the artifact to verify integrity and produce the measurement report.
//!
//! Hard opt-in: nothing here runs unless `FAKTOR_ECON_CAPTURE=1`. The lane
//! never runs in the normal test suite or in CI by default; it is wired into
//! the nightly `coding-benchmark-real-model` step behind that flag only.
//!
//! Configuration (never logged, keys never recorded):
//!
//! - `FAKTOR_ECON_CAPTURE=1` — enable the lane;
//! - `FAKTOR_ECON_LOCAL_MODEL` — Ollama model id (local lane; default base
//!   URL `http://127.0.0.1:11434`, override `FAKTOR_ECON_LOCAL_BASE_URL`);
//! - `FAKTOR_ECON_FRONTIER_MODEL` / `FAKTOR_ECON_FRONTIER_API_KEY` /
//!   `FAKTOR_ECON_FRONTIER_BASE_URL` (default `https://api.openai.com/v1`) —
//!   frontier lane; its EXACT price is required
//!   (`FAKTOR_ECON_FRONTIER_INPUT_MICRO_PER_MTOK` and
//!   `FAKTOR_ECON_FRONTIER_OUTPUT_MICRO_PER_MTOK`) or must be known to the
//!   provider catalog — the lane NEVER fabricates a price;
//! - `FAKTOR_ECON_CAPTURE_ID` / `FAKTOR_ECON_CAPTURE_OUT` — artifact id and
//!   output directory (default `frozen/<capture-id>/capture.json`).

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use faktor_core::model::{
    MicroUsdPerMillionTokens, ModelCapabilities, ModelDescriptor, ModelEconomics, ModelSource,
    PriceQuote, PricingSnapshot, PricingState,
};
use faktor_core::{CancellationToken, OpId, SessionId};
use faktor_ollama::{OllamaConfig, OllamaProvider};
use faktor_openai::{OpenAiConfig, OpenAiProvider};
use faktor_provider::egress::{EgressAddressPolicy, PolicyCheckedHttpTransport};
use faktor_provider::{
    ContentPart, GenericAgentRequest, Provider, ProviderChunk, RequestMessage, RequestMeta, Role,
};
use faktor_security::destination::DestinationPolicy;
use faktor_security::secret::SecretValue;
use futures::StreamExt;
use serde::Serialize;

use crate::coding::{coding_corpus_root, load_coding_corpus, CodingTaskSpec};
use crate::frozen::{
    Arm, CaptureSource, CostSource, FrozenCapture, FrozenModel, TokenBasis, WallTimeBasis,
};
use crate::pipeline::{run_arm, CodingModel, CompletionRequest, ModelCompletion};

pub const ENV_CAPTURE: &str = "FAKTOR_ECON_CAPTURE";
pub const ENV_LOCAL_MODEL: &str = "FAKTOR_ECON_LOCAL_MODEL";
pub const ENV_LOCAL_BASE_URL: &str = "FAKTOR_ECON_LOCAL_BASE_URL";
pub const ENV_FRONTIER_MODEL: &str = "FAKTOR_ECON_FRONTIER_MODEL";
pub const ENV_FRONTIER_API_KEY: &str = "FAKTOR_ECON_FRONTIER_API_KEY";
pub const ENV_FRONTIER_BASE_URL: &str = "FAKTOR_ECON_FRONTIER_BASE_URL";
pub const ENV_FRONTIER_INPUT_PRICE: &str = "FAKTOR_ECON_FRONTIER_INPUT_MICRO_PER_MTOK";
pub const ENV_FRONTIER_OUTPUT_PRICE: &str = "FAKTOR_ECON_FRONTIER_OUTPUT_MICRO_PER_MTOK";
pub const ENV_CAPTURE_ID: &str = "FAKTOR_ECON_CAPTURE_ID";
pub const ENV_CAPTURE_OUT: &str = "FAKTOR_ECON_CAPTURE_OUT";
pub const ENV_CALL_TIMEOUT_MS: &str = "FAKTOR_ECON_CALL_TIMEOUT_MS";

/// Default frontier base URL (OpenAI-compatible).
pub const DEFAULT_FRONTIER_BASE_URL: &str = "https://api.openai.com/v1";
/// Default native Ollama base URL.
pub const DEFAULT_LOCAL_BASE_URL: &str = "http://127.0.0.1:11434";
/// Default per-call deadline for one live model request.
pub const DEFAULT_CALL_TIMEOUT_MS: u64 = 600_000;
/// Absolute per-call deadline ceiling.
pub const MAX_CALL_TIMEOUT_MS: u64 = 3_600_000;

/// The local (Ollama) lane configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalLaneConfig {
    pub model: String,
    pub base_url: String,
}

/// The frontier (OpenAI-compatible) lane configuration. The key is held as a
/// `String` only until the adapter wraps it in `SecretValue`; it is never
/// recorded, printed or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierLaneConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    /// Exact microUSD per million tokens, resolved from the environment or
    /// the provider catalog. Never fabricated.
    pub input_micro_per_mtok: u64,
    pub output_micro_per_mtok: u64,
    /// Where the exact price came from (recorded; `env` or `catalog`).
    pub price_source: String,
}

/// The full capture configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    pub capture_id: String,
    pub output_dir: PathBuf,
    pub call_timeout_ms: u64,
    pub local: Option<LocalLaneConfig>,
    pub frontier: Option<FrontierLaneConfig>,
}

/// Parse the capture configuration from the environment. `Ok(None)` = the
/// lane is not enabled (the documented default).
pub fn capture_config_from_env() -> Result<Option<CaptureConfig>, String> {
    if env::var(ENV_CAPTURE).ok().as_deref() != Some("1") {
        return Ok(None);
    }
    // The local lane is optional: an unset/blank model env means the lane
    // is simply not enabled.
    let local = env::var(ENV_LOCAL_MODEL)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|model| LocalLaneConfig {
            model,
            base_url: env::var(ENV_LOCAL_BASE_URL)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_LOCAL_BASE_URL.to_string()),
        });
    let frontier = match env::var(ENV_FRONTIER_MODEL)
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(model) => {
            let api_key = env::var(ENV_FRONTIER_API_KEY).map_err(|_| {
                format!("{ENV_FRONTIER_API_KEY} must be set with {ENV_FRONTIER_MODEL}")
            })?;
            if api_key.trim().is_empty() {
                return Err(format!("{ENV_FRONTIER_API_KEY} must not be empty"));
            }
            let env_input = env::var(ENV_FRONTIER_INPUT_PRICE).ok();
            let env_output = env::var(ENV_FRONTIER_OUTPUT_PRICE).ok();
            let (input, output, price_source) = match (env_input, env_output) {
                (Some(i), Some(o)) => {
                    let i = i.trim().parse::<u64>().map_err(|_| {
                        format!("{ENV_FRONTIER_INPUT_PRICE} must be an integer microUSD/Mtok value")
                    })?;
                    let o = o.trim().parse::<u64>().map_err(|_| {
                        format!("{ENV_FRONTIER_OUTPUT_PRICE} must be an integer microUSD/Mtok value")
                    })?;
                    (i, o, "env".to_string())
                }
                (None, None) => {
                    let (i, o) = frontier_catalog_price(&model)?.ok_or_else(|| {
                        format!(
                            "no exact frontier price: set {ENV_FRONTIER_INPUT_PRICE}/{ENV_FRONTIER_OUTPUT_PRICE} \
                             (microUSD per million tokens) — the capture never fabricates prices"
                        )
                    })?;
                    (i, o, "catalog".to_string())
                }
                _ => {
                    return Err(format!(
                        "{ENV_FRONTIER_INPUT_PRICE} and {ENV_FRONTIER_OUTPUT_PRICE} must be set together"
                    ))
                }
            };
            Some(FrontierLaneConfig {
                model,
                base_url: env::var(ENV_FRONTIER_BASE_URL)
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_FRONTIER_BASE_URL.to_string()),
                api_key,
                input_micro_per_mtok: input,
                output_micro_per_mtok: output,
                price_source,
            })
        }
        None => None,
    };
    if local.is_none() && frontier.is_none() {
        return Err(format!(
            "capture enabled but no lane configured: set {ENV_LOCAL_MODEL} and/or {ENV_FRONTIER_MODEL}"
        ));
    }
    let call_timeout_ms = match env::var(ENV_CALL_TIMEOUT_MS).ok() {
        Some(raw) => {
            let ms = raw
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("{ENV_CALL_TIMEOUT_MS} must be an integer"))?;
            if ms == 0 || ms > MAX_CALL_TIMEOUT_MS {
                return Err(format!(
                    "{ENV_CALL_TIMEOUT_MS} must be 1..={MAX_CALL_TIMEOUT_MS}"
                ));
            }
            ms
        }
        None => DEFAULT_CALL_TIMEOUT_MS,
    };
    let capture_id = env::var(ENV_CAPTURE_ID)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("live-{}", unix_secs()));
    validate_capture_id(&capture_id)?;
    let output_dir = env::var(ENV_CAPTURE_OUT)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("frozen")
                .join(&capture_id)
        });
    Ok(Some(CaptureConfig {
        capture_id,
        output_dir,
        call_timeout_ms,
        local,
        frontier,
    }))
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn validate_capture_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err("capture id must be [A-Za-z0-9._-] and <= 64 bytes".into());
    }
    Ok(())
}

/// Look up an exact built-in catalog price for a frontier model by probing
/// each provider family's builtin table. `None` = unknown; the caller then
/// requires explicit environment prices.
fn frontier_catalog_price(model: &str) -> Result<Option<(u64, u64)>, String> {
    for family in ["openai", "anthropic", "google", "deepseek"] {
        if let Some(row) = faktor_provider::catalog::builtin::lookup(family, model) {
            let quote = faktor_provider::catalog::builtin::quote_of(&row);
            let input = quote.input.0;
            let output = quote.output.0;
            if input > 0 && output > 0 {
                return Ok(Some((input, output)));
            }
        }
    }
    Ok(None)
}

fn exact_pricing(input: u64, output: u64, source: &str) -> PricingState {
    PricingState::Known(PricingSnapshot::exact(
        PriceQuote {
            input: MicroUsdPerMillionTokens(input),
            output: MicroUsdPerMillionTokens(output),
            cache_read: MicroUsdPerMillionTokens(input / 4),
            cache_write: MicroUsdPerMillionTokens(input / 2),
        },
        1,
        source.to_string(),
    ))
}

fn descriptor_for(
    provider: &str,
    model: &str,
    caps: &ModelCapabilities,
    local: bool,
) -> ModelDescriptor {
    let economics = if local {
        ModelEconomics {
            estimated_latency_ms: 3000,
            tool_reliability: 78,
            reasoning_reliability: 76,
            coding_reliability: 80,
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
        context: caps.context as u64,
        max_output: caps.max_output as u64,
        tools: caps.tools,
        parallel_tools: caps.parallel_tools,
        reasoning: caps.reasoning,
        thinking: caps.thinking,
        vision: caps.vision,
        structured_output: caps.json_schema,
        embeddings: caps.embeddings,
        streaming: caps.streaming,
        economics,
        source: ModelSource::UserOverride,
    }
}

// ------------------------------------------------------------- live models

/// One real model behind a [`CodingModel`] implementation: the request is
/// lowered through the REAL provider adapter over the policy-checked egress
/// transport.
pub struct LiveModel {
    routed: FrozenModel,
    provider: Arc<dyn Provider>,
    runtime: tokio::runtime::Runtime,
    timeout_ms: u64,
}

impl LiveModel {
    pub fn new(
        routed: FrozenModel,
        provider: Arc<dyn Provider>,
        timeout_ms: u64,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("tokio runtime: {e}"))?;
        Ok(LiveModel {
            routed,
            provider,
            runtime,
            timeout_ms,
        })
    }

    /// Read the full prompt as ONE user message.
    fn request(
        &self,
        prompt: &crate::pipeline::PromptArtifacts,
        attempt: u32,
    ) -> GenericAgentRequest {
        GenericAgentRequest {
            model: self.routed.descriptor.model.clone(),
            system: prompt.system.clone(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec![ContentPart::text(prompt.user.clone())],
            }],
            tools: Vec::new(),
            max_output: Some(self.routed.descriptor.max_output.min(16_384) as usize),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: OpId::new(1),
                session_id: SessionId::new(1),
                provider: self.routed.descriptor.provider.clone(),
                attempt,
                deadline_ms: self.timeout_ms,
                cancellation: CancellationToken::new(),
            },
        }
    }
}

impl CodingModel for LiveModel {
    fn routed(&self) -> &FrozenModel {
        &self.routed
    }

    fn complete(&self, request: &CompletionRequest<'_>) -> Result<ModelCompletion, String> {
        let prompt = request.prompt();
        let req = self.request(&prompt, request.attempt);
        let started = Instant::now();
        let provider = self.provider.clone();
        let collected = self.runtime.block_on(async move {
            let mut stream = provider.stream(req);
            let mut text = String::new();
            let mut usage: Option<faktor_provider::CanonicalUsage> = None;
            while let Some(chunk) = stream.next().await {
                match chunk.map_err(|e| format!("provider stream failed: {e}"))? {
                    ProviderChunk::Text { text: delta } => text.push_str(&delta),
                    ProviderChunk::Usage(frame) => usage = Some(frame),
                    ProviderChunk::Done => break,
                    ProviderChunk::Reasoning { .. } | ProviderChunk::ToolCall { .. } => {}
                }
            }
            Ok::<_, String>((text, usage))
        })?;
        let latency_ms = started.elapsed().as_millis() as u64;
        let (text, usage) = collected;
        let usage = usage.unwrap_or_default();
        let tokens_in = usage
            .uncached_input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens);
        let tokens_out = usage.output_tokens;
        let (spend_micro, cost_source) = self.settle(&usage, tokens_in, tokens_out)?;
        Ok(ModelCompletion {
            text,
            tokens_in,
            tokens_out,
            spend_micro,
            cost_source,
            latency_ms,
        })
    }
}

impl LiveModel {
    fn settle(
        &self,
        usage: &faktor_provider::CanonicalUsage,
        tokens_in: u64,
        tokens_out: u64,
    ) -> Result<(u64, CostSource), String> {
        if let Some(reported) = &usage.reported_cost {
            if reported.is_usd() {
                return Ok((reported.micro_usd, CostSource::ProviderReported));
            }
        }
        if self.routed.local {
            return Ok((0, CostSource::LocalZero));
        }
        let settled = self
            .routed
            .pricing
            .snapshot()
            .settle_cost(tokens_in, 0, 0, tokens_out)
            .ok_or_else(|| {
                "frozen frontier pricing cannot settle the recorded usage (unknown price)"
                    .to_string()
            })?;
        Ok((settled, CostSource::PriceTable))
    }
}

// ------------------------------------------------------------ build + run

fn host_rule(base_url: &str) -> Result<(String, bool), String> {
    let rest = base_url
        .strip_prefix("https://")
        .map(|r| (r, "https://"))
        .or_else(|| base_url.strip_prefix("http://").map(|r| (r, "http://")))
        .ok_or_else(|| format!("base url {base_url:?} must be http:// or https://"))?;
    let (authority, _path) = match rest.0.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest.0, ""),
    };
    if authority.is_empty() || authority.contains('@') || authority.contains('[') {
        return Err(format!(
            "base url {base_url:?} has an unsupported authority"
        ));
    }
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err(format!("base url {base_url:?} has no host"));
    }
    let loopback = host == "localhost" || host == "::1" || host.starts_with("127.");
    let rule = format!("{}{}", rest.1, authority);
    Ok((rule, loopback))
}

fn build_transport(
    base_url: &str,
) -> Result<Arc<dyn faktor_provider::egress::HttpTransport>, String> {
    let (rule, loopback) = host_rule(base_url)?;
    let policy = DestinationPolicy::parse_lines([rule])
        .map_err(|e| format!("destination policy for {base_url:?}: {e}"))?;
    let addresses = if loopback {
        EgressAddressPolicy::LOCAL
    } else {
        EgressAddressPolicy::EXTERNAL
    };
    let transport =
        PolicyCheckedHttpTransport::try_with_policy_scan_and_addresses(policy, None, addresses)
            .map_err(|e| format!("egress client for {base_url:?}: {e}"))?;
    Ok(Arc::new(transport))
}

/// The live model table a capture run drives: the dispatchable models plus
/// their frozen (wire-stable) descriptors.
pub type LiveModelTable = (Vec<Arc<dyn CodingModel>>, Vec<FrozenModel>);

/// Build the live model table from the capture configuration.
pub fn build_live_models(config: &CaptureConfig) -> Result<LiveModelTable, String> {
    let mut models: Vec<Arc<dyn CodingModel>> = Vec::new();
    let mut table: Vec<FrozenModel> = Vec::new();
    if let Some(local) = &config.local {
        let transport = build_transport(&local.base_url)?;
        let provider =
            OllamaProvider::new(OllamaConfig::new(Some(local.base_url.clone())), transport);
        let dyn_provider: Arc<dyn Provider> = provider.clone();
        let entry = dyn_provider.catalog_entry(&local.model);
        let frozen = FrozenModel {
            descriptor: descriptor_for("ollama", &local.model, &entry.capabilities, true),
            pricing: PricingState::LocalZero,
            local: true,
            price_source: "ollama:local_zero".into(),
        };
        models.push(Arc::new(LiveModel::new(
            frozen.clone(),
            dyn_provider,
            config.call_timeout_ms,
        )?));
        table.push(frozen);
    }
    if let Some(frontier) = &config.frontier {
        let transport = build_transport(&frontier.base_url)?;
        let secret = SecretValue::new(frontier.api_key.clone());
        let cfg = OpenAiConfig::chat(frontier.base_url.clone(), Some(secret));
        let provider = OpenAiProvider::build(cfg, transport);
        let entry = provider.catalog_entry(&frontier.model);
        let pricing = exact_pricing(
            frontier.input_micro_per_mtok,
            frontier.output_micro_per_mtok,
            &format!("econ-bench-capture-{}", frontier.price_source),
        );
        let frozen = FrozenModel {
            descriptor: descriptor_for("openai", &frontier.model, &entry.capabilities, false),
            pricing,
            local: false,
            price_source: frontier.price_source.clone(),
        };
        models.push(Arc::new(LiveModel::new(
            frozen.clone(),
            provider,
            config.call_timeout_ms,
        )?));
        table.push(frozen);
    }
    if models.is_empty() {
        return Err("no capture lane configured".into());
    }
    Ok((models, table))
}

/// The outcome of one live capture (artifact written + replay verified).
#[derive(Debug, Serialize)]
pub struct CaptureOutcome {
    pub capture_path: String,
    pub report_path: String,
    pub capture_id: String,
    pub runs: usize,
    pub verified: usize,
    pub source: String,
}

/// Run the full live capture: both arms over the corpus through the real
/// pipeline, then the deterministic replay (integrity verification, wired
/// outcome ledger, measurement report) over the freshly frozen artifact.
pub fn run_capture(config: &CaptureConfig) -> Result<(FrozenCapture, CaptureOutcome), String> {
    // The frontier lane's configured env prices must be exactly what the
    // frozen table carries.
    let corpus = load_coding_corpus(&coding_corpus_root())?;
    let (models, table) = build_live_models(config)?;
    let scratch = tempfile::TempDir::new().map_err(|e| crate::io_err("capture scratch", e))?;
    let mut runs = Vec::new();
    for arm in Arm::ALL {
        let now = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        };
        let mut arm_runs = run_arm(
            &corpus,
            arm,
            &models,
            scratch.path(),
            CaptureSource::Live,
            &|_task: &CodingTaskSpec| now(),
        )?;
        runs.append(&mut arm_runs);
    }
    let created_label = format!("live capture at unix {}s", unix_secs());
    let capture = FrozenCapture {
        schema: crate::frozen::FROZEN_SCHEMA.to_string(),
        capture_id: config.capture_id.clone(),
        source: CaptureSource::Live,
        engine: format!(
            "faktor-tests-router-economics {}",
            env!("CARGO_PKG_VERSION")
        ),
        created_label,
        corpus_digest: corpus.digest.clone(),
        token_basis: TokenBasis::ProviderUsage,
        wall_time_basis: WallTimeBasis::Captured,
        models: table,
        runs,
        digest: String::new(),
    }
    .finish()?;
    let path = config.output_dir.join("capture.json");
    crate::frozen::save_capture(&capture, &path)
        .map_err(|e| crate::io_err("write capture artifact", e))?;
    // Immediately replay the frozen artifact: integrity re-verification,
    // routing reproduction, durable ledger + measurement report. A capture
    // whose replay fails is not a valid capture.
    let report = crate::replay::replay_capture(&capture, &corpus, scratch.path())?;
    let report_path = config.output_dir.join("report.json");
    crate::replay::write_replay_report(
        &crate::replay::ReplayReport {
            schema: crate::replay::REPLAY_SCHEMA.into(),
            engine: capture.engine.clone(),
            fixtures_only: false,
            captures: vec![report],
        },
        &report_path,
    )
    .map_err(|e| crate::io_err("write capture report", e))?;
    let verified = capture.runs.iter().filter(|r| r.verified).count();
    let outcome = CaptureOutcome {
        capture_path: path.display().to_string(),
        report_path: report_path.display().to_string(),
        capture_id: capture.capture_id.clone(),
        runs: capture.runs.len(),
        verified,
        source: "live".into(),
    };
    Ok((capture, outcome))
}
