//! faktor-ollama — native Ollama adapter (spec §10).
//!
//! Discovery via `GET /api/tags` (never a hard-coded list — `ollama pull
//! qwen3.8` makes it appear automatically), capability probing via
//! `GET /api/show`, native `/api/chat` streaming with tools/thinking/
//! keep_alive, and `/api/embed` embeddings. OpenAI-compatible mode is a
//! fallback only.
//!
//! Wire shapes are Ollama-native (`docs/api.md`), never OpenAI-style
//! content arrays: `message` objects carry `role`, a plain-string
//! `content`, optional `thinking`/`images`/`tool_calls`; a tool round trip
//! is an assistant message with `tool_calls` followed by role-`tool`
//! messages; the thinking knob is the top-level `/api/chat` `think`
//! parameter (boolean or `"low"`/`"medium"`/`"high"` level).

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use faktor_core::error::{Error, ErrorKind};
use faktor_core::model::{ModelCapabilities, ReasoningMode};
use faktor_provider::catalog::{
    ModelCatalogEntry, PricingState, Provenance, QualityPrior, CATALOG_FIRST_EPOCH,
};
use faktor_provider::egress::{
    execute_get, execute_post_json, HttpTransport, PolicyCheckedHttpTransport,
};
use faktor_provider::transport::{
    guarded_lines, utf8_line_stream, StreamDeadlines, MAX_LINE_BYTES, PROVIDER_CEILING_MS,
};
use futures::Stream;

/// Map a transport-level refusal onto the ollama host-facing error type.
/// A genuine transport failure stays a retryable `Network` error (exactly
/// what the raw client produced before); anything the policy/build layer
/// refused (denied destination, unparseable URL) is a non-retryable
/// `Provider` error — retrying it can never succeed.
fn egress_to_host_error(e: faktor_provider::egress::EgressError) -> Error {
    let kind = match &e {
        faktor_provider::egress::EgressError::Transport(_) => ErrorKind::Network,
        _ => ErrorKind::Provider {
            code: "egress".into(),
            retryable: false,
        },
    };
    Error::new(kind, e.to_string())
}

/// Stream hang controls: first-byte / idle bounds from the transport
/// defaults (audit round 9). The OVERALL bound now rides the operation
/// deadline the runtime stamped into `RequestMeta::deadline_ms` (audit
/// round 15): `0` keeps streams unbounded overall (defaults only), any
/// positive value caps the stream's whole lifetime at
/// `min(deadline_ms, PROVIDER_CEILING_MS)` — a stuck daemon can never
/// outlive the operation that started the request.
fn stream_deadlines(request: &GenericAgentRequest) -> StreamDeadlines {
    let mut deadlines = StreamDeadlines::default();
    if request.meta.deadline_ms > 0 {
        deadlines.overall_ms = request.meta.deadline_ms.min(PROVIDER_CEILING_MS);
    }
    deadlines
}
use faktor_provider::{
    CanonicalUsage, ContentKind, EmbeddingRequest, EmbeddingResponse, GenericAgentRequest,
    Provider, ProviderChunk, ProviderError, ProviderErrorKind, ProviderStream, RequestMessage,
    Role,
};

const DEFAULT_BASE: &str = "http://127.0.0.1:11434";

/// Per-image raw-byte ceiling the Ollama adapter delivers. Ollama's native
/// `/api/chat` takes base64 `images` without documenting a per-image limit;
/// the daemon-wide default ([`faktor_provider::MAX_MODEL_IMAGE_BYTES`]) is
/// therefore the adapter's own bound, still capped structurally by
/// [`faktor_provider::MAX_MEDIA_BYTES_HARD`] inside the delivery gate.
pub const OLLAMA_MAX_IMAGE_BYTES: usize = faktor_provider::MAX_MODEL_IMAGE_BYTES;

/// Hard bound on the RAW bytes of ONE `/api/embed` response body. A legal
/// response is `inputs (≤ 64) × dimensions (≤ 8192) × 4` bytes plus JSON
/// overhead (< 3 MiB); 8 MiB admits every legal shape while a hostile
/// daemon cannot stream an unbounded body into RAM.
pub const EMBED_RESPONSE_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OllamaConfig {
    pub base_url: String,
    /// Keep the model loaded for this duration (native keep_alive).
    pub keep_alive: Option<String>,
    /// Capability overrides per model (probed values win unless overridden).
    pub model_overrides: HashMap<String, ModelCapabilities>,
}

impl OllamaConfig {
    pub fn new(base_url: Option<String>) -> Self {
        Self {
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE.to_string()),
            keep_alive: Some("30m".into()),
            model_overrides: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
struct TagsResponse {
    models: Vec<TagModel>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct TagModel {
    name: String,
    #[allow(dead_code)]
    model: Option<String>,
    #[allow(dead_code)]
    size: Option<u64>,
    #[allow(dead_code)]
    details: Option<ModelDetails>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ModelDetails {
    #[allow(dead_code)]
    family: Option<String>,
    #[allow(dead_code)]
    parameter_size: Option<String>,
    #[allow(dead_code)]
    quantization_level: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct ShowResponse {
    model_info: Option<serde_json::Value>,
    capabilities: Option<Vec<String>>,
    #[allow(dead_code)]
    details: Option<ModelDetails>,
    #[allow(dead_code)]
    parameters: Option<serde_json::Value>,
}

/// Context-window facts for one model: the model's maximum (from
/// `/api/show` — the same number `probe_model` reports as
/// `ModelCapabilities::context`) and the runtime-effective window after the
/// `/api/ps` allocation is applied: `min(model_max, allocated)`, `None`
/// while the model is not loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaContext {
    pub model_max: usize,
    pub runtime_effective: Option<usize>,
}

pub struct OllamaProvider {
    config: OllamaConfig,
    transport: Arc<dyn HttpTransport>,
    /// Live-probed capabilities (spec §10: discovery/probing drive behavior
    /// — never a hard-coded list). Written by [`refresh_from_live`] and read
    /// by `capabilities()`; empty until the daemon warms the provider.
    probed: std::sync::RwLock<HashMap<String, ModelCapabilities>>,
    /// Cached runtime context limit per model (P0 "Qwen3.8 advertises 256K
    /// but is allocated 64K"): `min(model max from /api/show, /api/ps
    /// allocation)` for models the daemon reported as LOADED. Written by
    /// [`refresh_from_live`]'s `/api/ps` pass (and by the live
    /// `runtime_context`/`effective_context` probes); read synchronously by
    /// [`Provider::runtime_context_limit`] so the agent budgets from the
    /// REAL loaded window. Absent entries mean "no live /api/ps data": the
    /// agent then budgets from the model maximum (today's behavior — the
    /// safe direction when the daemon never reported an allocation).
    runtime_limits: std::sync::RwLock<HashMap<String, usize>>,
    /// Per-provider-response sequence: tool ids are
    /// `ollama:<response_seq>:<tool_index>` so ids stay unique across every
    /// response a session streams (see [`OllamaProvider::stream`]).
    response_seq: AtomicU64,
}

impl OllamaProvider {
    /// Concrete constructor (for discovery/probing APIs).
    pub fn new(config: OllamaConfig) -> Arc<Self> {
        Self::new_with_transport(config, Arc::new(PolicyCheckedHttpTransport::permissive()))
    }

    /// Concrete constructor with an injected transport (policy-checked in
    /// production, mock in tests).
    pub fn new_with_transport(
        config: OllamaConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            transport,
            probed: std::sync::RwLock::new(HashMap::new()),
            runtime_limits: std::sync::RwLock::new(HashMap::new()),
            response_seq: AtomicU64::new(0),
        })
    }

    /// Live capability warm-up (spec §10): `GET /api/tags` discovers the
    /// installed models, each is probed via `GET /api/show`, and the results
    /// drive `capabilities()` from then on. Bounded: at most
    /// `MAX_PROBED_MODELS` models, 10s per probe. A failed probe leaves the
    /// model on its default capabilities; discovery failure surfaces. The
    /// `/api/ps` pass ([`refresh_runtime_contexts`]) follows: best-effort, a
    /// dead/hostile `/api/ps` is warned and the runtime-context cache stays
    /// as it was — the budget falls back to model maxima, never an error
    /// here.
    pub async fn refresh_from_live(&self) -> Result<usize, Error> {
        const MAX_PROBED_MODELS: usize = 64;
        let models = self.discover_models().await?;
        let mut map: HashMap<String, ModelCapabilities> = HashMap::new();
        for model in models.iter().take(MAX_PROBED_MODELS) {
            match tokio::time::timeout(std::time::Duration::from_secs(10), self.probe_model(model))
                .await
            {
                Ok(Ok(caps)) => {
                    map.insert(model.clone(), caps);
                }
                Ok(Err(e)) => {
                    tracing::warn!("ollama probe {model} failed: {e}");
                }
                Err(_) => {
                    tracing::warn!("ollama probe {model} timed out");
                }
            }
        }
        let n = map.len();
        *self.probed.write().unwrap() = map;
        // P0: the /api/ps pass fills the runtime-context cache from the
        // freshly probed maxima. Warn-only — a hostile daemon must not fail
        // warm-up, and a stale-but-conservative cache beats no cache.
        if let Err(e) = self.refresh_runtime_contexts().await {
            tracing::warn!(
                "ollama /api/ps refresh failed (budget falls back to model maxima): {e}"
            );
        }
        Ok(n)
    }

    /// `GET /api/ps` refresh pass for the runtime-context cache: every
    /// probed model that the daemon reports as loaded gets
    /// `min(model max from /api/show, /api/ps allocation)` cached for
    /// [`Provider::runtime_context_limit`]; models no longer loaded are
    /// evicted from the cache (no live allocation → model-max fallback). A
    /// dead or hostile `/api/ps` is a loud error and leaves the previous
    /// cache untouched (stale-but-conservative, never cleared). Returns how
    /// many models now have a cached limit.
    pub async fn refresh_runtime_contexts(&self) -> Result<usize, Error> {
        let resp = execute_get(
            self.transport.as_ref(),
            &format!("{}/api/ps", self.config.base_url),
        )
        .await
        .map_err(egress_to_host_error)?;
        if !resp.status().is_success() {
            return Err(Error::new(
                ErrorKind::Provider {
                    code: resp.status().as_u16().to_string(),
                    retryable: false,
                },
                format!("ollama /api/ps returned {}", resp.status()),
            ));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::new(ErrorKind::Malformed, format!("ollama ps body: {e}")))?;
        let probed = self.probed.read().unwrap().clone();
        let mut updated = 0usize;
        for (model, caps) in &probed {
            // Hostile bodies are loud, never a silent partial update: one
            // corrupt /api/ps body aborts the whole pass (the parse outcome
            // is per-body, not per-model, so no earlier writes happened).
            let allocated = ps_allocated_context(&body, model)?.map(|c| c as usize);
            self.store_ps_observation(model, caps, allocated);
            if allocated.is_some() {
                updated += 1;
            }
        }
        Ok(updated)
    }

    /// Cache one `/api/ps` observation: `min(model max, allocation)` when
    /// the model is loaded; REMOVE the entry when it is not (no live data —
    /// the caller falls back to the model maximum, safe direction).
    fn store_ps_observation(
        &self,
        model: &str,
        caps: &ModelCapabilities,
        allocated: Option<usize>,
    ) {
        let mut limits = self.runtime_limits.write().unwrap();
        match allocated {
            Some(allocated) => {
                limits.insert(model.to_string(), caps.context.min(allocated));
            }
            None => {
                limits.remove(model);
            }
        }
    }

    pub fn build(config: OllamaConfig) -> Arc<dyn Provider> {
        Self::new(config)
    }

    /// Discover installed models (spec §10): `GET /api/tags`.
    pub async fn discover_models(&self) -> Result<Vec<String>, Error> {
        let resp = execute_get(
            self.transport.as_ref(),
            &format!("{}/api/tags", self.config.base_url),
        )
        .await
        .map_err(egress_to_host_error)?;
        if !resp.status().is_success() {
            return Err(Error::new(
                ErrorKind::Provider {
                    code: resp.status().as_u16().to_string(),
                    retryable: false,
                },
                format!("ollama tags returned {}", resp.status()),
            ));
        }
        let tags: TagsResponse = resp
            .json()
            .await
            .map_err(|e| Error::new(ErrorKind::Malformed, format!("ollama tags body: {e}")))?;
        let mut names: Vec<String> = tags.models.into_iter().map(|m| m.name).collect();
        names.sort();
        Ok(names)
    }

    /// Probe a model's capabilities via `GET /api/show` (spec §10).
    pub async fn probe_model(&self, model: &str) -> Result<ModelCapabilities, Error> {
        let resp = execute_post_json(
            self.transport.as_ref(),
            &format!("{}/api/show", self.config.base_url),
            reqwest::header::HeaderMap::new(),
            &serde_json::json!({ "name": model, "verbose": true }),
        )
        .await
        .map_err(egress_to_host_error)?;
        if !resp.status().is_success() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("ollama cannot see model {model}"),
            ));
        }
        let show: ShowResponse = resp
            .json()
            .await
            .map_err(|e| Error::new(ErrorKind::Malformed, format!("ollama show body: {e}")))?;
        Ok(caps_from_show(model, &show))
    }

    /// `GET /api/ps`: the context window currently ALLOCATED for `model` in
    /// the loaded process. `Ok(None)` when the model is not loaded (or the
    /// daemon does not report an allocation); hostile bodies are loud
    /// errors, never a panic.
    pub async fn ps_allocated(&self, model: &str) -> Result<Option<usize>, Error> {
        let resp = execute_get(
            self.transport.as_ref(),
            &format!("{}/api/ps", self.config.base_url),
        )
        .await
        .map_err(egress_to_host_error)?;
        if !resp.status().is_success() {
            return Err(Error::new(
                ErrorKind::Provider {
                    code: resp.status().as_u16().to_string(),
                    retryable: false,
                },
                format!("ollama /api/ps returned {}", resp.status()),
            ));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::new(ErrorKind::Malformed, format!("ollama ps body: {e}")))?;
        Ok(ps_allocated_context(&body, model)?.map(|c| c as usize))
    }

    /// Probe both context numbers for `model`: the model maximum
    /// (`/api/show`, exactly what `probe_model` reports as
    /// `ModelCapabilities::context`) and the runtime-effective window
    /// (`/api/ps`, `min(model_max, allocated)`; `None` while unloaded).
    /// Also refreshes the cached runtime-context limit for `model` (the
    /// value [`Provider::runtime_context_limit`] serves synchronously).
    pub async fn runtime_context(&self, model: &str) -> Result<OllamaContext, Error> {
        let caps = self.probe_model(model).await?;
        let allocated = self.ps_allocated(model).await?;
        let runtime_effective = allocated.map(|allocated| caps.context.min(allocated));
        self.store_ps_observation(model, &caps, allocated);
        Ok(OllamaContext {
            model_max: caps.context,
            runtime_effective,
        })
    }

    /// Effective context = `min(model max from /api/show, allocated from
    /// /api/ps)`; `Ok(None)` when the model is not currently loaded. The
    /// observation also lands in the [`Provider::runtime_context_limit`]
    /// cache.
    pub async fn effective_context(&self, model: &str) -> Result<Option<usize>, Error> {
        Ok(self.runtime_context(model).await?.runtime_effective)
    }

    /// Native wire serializer (P0 "Qwen3.8 not first-class"): the generic
    /// content-block model is lowered into real Ollama message objects,
    /// never OpenAI-style `{"type": ...}` content arrays. Shapes follow
    /// Ollama's `/api/chat` contract (`docs/api.md`): each message carries
    /// `role` + plain-string `content`, optional `thinking` (assistant
    /// reasoning), optional base64 `images`, optional `tool_calls`
    /// (`{"function": {"name", "arguments"}}`); tool results are role
    /// `tool` messages (with the documented `tool_name` when the answered
    /// call is visible in this request). `GenericAgentRequest::system`
    /// becomes the first `role: system` message; images are omitted when
    /// the request carries none.
    fn wire_body(&self, req: &GenericAgentRequest) -> serde_json::Value {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        if !req.system.is_empty() {
            messages.push(serde_json::json!({ "role": "system", "content": req.system }));
        }
        // tool_call id -> tool name seen so far in this request: role-"tool"
        // messages can carry the native tool_name only when the call it
        // answers was part of this request.
        let mut call_names: HashMap<String, String> = HashMap::new();
        for m in &req.messages {
            messages.extend(lower_native_message(m, &mut call_names));
        }
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();
        let mut body = serde_json::json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        if let Some(keep) = &self.config.keep_alive {
            body["keep_alive"] = serde_json::json!(keep);
        }
        if let Some(max_out) = req.max_output {
            body["options"] = serde_json::json!({ "num_predict": max_out });
        }
        // Native thinking knob: driven by the request's stored
        // ReasoningMode (never guessed from text). Off -> think:false;
        // Low/Medium/High -> the documented effort level when the model
        // profile indicates effort-level reasoning (caps.reasoning), else
        // the boolean knob (caps.thinking); omitted entirely when the
        // profile supports neither.
        if let Some(mode) = req.reasoning {
            if let Some(knob) = thinking_knob(mode, &self.capabilities(&req.model)) {
                body["think"] = knob;
            }
        }
        body
    }

    /// Native `/api/embed` wire body: the model, the ordered batch input,
    /// and the configured `keep_alive` — never any internal metadata
    /// (operation/session/deadline/cancellation stay off the wire like
    /// every other adapter call).
    fn embed_wire_body(&self, req: &EmbeddingRequest) -> serde_json::Value {
        // Typed struct (fixed field order): a serde_json::Map's key order
        // depends on workspace feature unification (`preserve_order` flips
        // it globally), so byte-exact wire assertions must not rely on Map
        // ordering.
        #[derive(serde::Serialize)]
        struct EmbedBody<'a> {
            model: &'a str,
            input: &'a [String],
            #[serde(skip_serializing_if = "Option::is_none")]
            keep_alive: Option<&'a str>,
        }
        serde_json::to_value(EmbedBody {
            model: &req.model,
            input: &req.inputs,
            keep_alive: self.config.keep_alive.as_deref(),
        })
        .unwrap_or(serde_json::Value::Null)
    }
}

impl Provider for OllamaProvider {
    fn id(&self) -> &str {
        "ollama"
    }

    fn known_models(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for k in self.config.model_overrides.keys() {
            out.push(k.clone());
        }
        for k in self.probed.read().unwrap().keys() {
            if !out.contains(k) {
                out.push(k.clone());
            }
        }
        if !out.contains(&"default".to_string()) {
            out.push("default".into());
        }
        out.sort();
        out
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(caps) = self.config.model_overrides.get(model) {
            return caps.clone();
        }
        if let Some(caps) = self.probed.read().unwrap().get(model) {
            return caps.clone();
        }
        // Default profile: small local models are the norm (spec §9/§10);
        // a live probe replaces this once the daemon warms the provider.
        ModelCapabilities::small_local()
    }

    fn catalog_entry(&self, model: &str) -> ModelCatalogEntry {
        // Audit P0-1: an Ollama runtime IS the local machine — zero
        // monetary cost is the measured truth (LocalZero), never a
        // missing price. Latency/reliability still count through the
        // conservative generic priors.
        ModelCatalogEntry {
            provider: self.id().to_string(),
            model: model.to_string(),
            capabilities: self.capabilities(model),
            pricing: PricingState::LocalZero,
            quality_prior: QualityPrior::default(),
            source_epoch: CATALOG_FIRST_EPOCH,
            provenance: Provenance::ProviderCatalog,
        }
    }

    /// The CACHED runtime window for `model` (P0 "the /api/ps allocation
    /// reaches the real budget"): `min(model max from /api/show, /api/ps
    /// allocation)` from the last successful [`refresh_from_live`] — the
    /// agent budgets from THIS, never from the raw advertised maximum.
    /// `None` when `/api/ps` never succeeded for the model (or the model is
    /// not loaded): the agent falls back to the model maximum, safe
    /// direction.
    fn runtime_context_limit(&self, model: &str) -> Option<usize> {
        self.runtime_limits.read().unwrap().get(model).copied()
    }

    fn max_image_bytes(&self) -> usize {
        OLLAMA_MAX_IMAGE_BYTES
    }

    /// The native `/api/chat` message object has NO document field (only
    /// `content`, `thinking`, `images` and `tool_calls`): Ollama is never
    /// document-capable, so a resolved [`ContentKind::FileData`] part is a
    /// typed refusal at the adapter boundary ([`Provider::stream`]) instead
    /// of a silent drop.
    fn document_capable(&self, _model: &str) -> bool {
        false
    }

    /// The probed `/api/show` capability drives the embedding gate (spec
    /// §10: discovery/probing drive behavior): an embedding-capable model
    /// (or an operator override) admits the CLI's strict `[embeddings]`
    /// selection; anything else refuses honestly, never a fabricated
    /// vector. Real daemons report both `embedding` and `embeddings`
    /// spellings — the probe accepts either.
    fn supports_embeddings(&self, model: &str) -> bool {
        self.capabilities(model).embeddings
    }

    /// ONE bounded `/api/embed` call (native API): the whole
    /// [`EmbeddingRequest`] is lowered to the native wire shape
    /// (`{"model", "input": [..]}` plus the configured `keep_alive`) and
    /// exactly one finite vector per input is returned. The call is
    /// synchronous by trait contract; it is bridged onto a bounded async
    /// execution (see [`run_embedding_future`]) and the operation's
    /// remaining deadline from [`EmbeddingRequest::meta`]
    /// (`RequestMeta::deadline_ms`) caps the ENTIRE call — headers AND body;
    /// without a deadline the transport's first-byte default is the bound.
    /// Every failure is a typed [`ProviderError`] whose retryability is the
    /// status/transport class (429/5xx/network retryable; 4xx and
    /// policy/build refusals terminal), so the configured embedder retries
    /// exactly per policy and never more.
    fn embed(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, ProviderError> {
        // Model is REQUIRED: a directly-constructed hostile request must
        // never lower an empty model onto the wire. The bounded batch is
        // re-validated here (defense in depth): `EmbeddingRequest::new`
        // refuses unbounded batches, but its fields are public, so the
        // adapter re-applies the provider's own bounds before lowering.
        if req.model.trim().is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::BadRequest,
                "ollama embeddings require a non-empty model",
            ));
        }
        validate_embedding_batch(&req.inputs)?;
        let body = self.embed_wire_body(&req);
        let url = format!("{}/api/embed", self.config.base_url);
        let transport = self.transport.clone();
        let bound_ms = if req.deadline_ms() > 0 {
            req.deadline_ms().min(PROVIDER_CEILING_MS)
        } else {
            // A non-streaming POST has no idle/first-byte guard of its own:
            // the transport's first-byte default is the fallback bound.
            StreamDeadlines::default().first_byte_ms
        };
        run_embedding_future(async move {
            let call = async {
                let resp = execute_post_json(
                    transport.as_ref(),
                    &url,
                    reqwest::header::HeaderMap::new(),
                    &body,
                )
                .await
                .map_err(ProviderError::from)?;
                let status = resp.status();
                if !status.is_success() {
                    let text = read_body_bounded(resp, EMBED_RESPONSE_MAX_BYTES)
                        .await
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_default();
                    return Err(status_to_provider_error(status, text));
                }
                let bytes = read_body_bounded(resp, EMBED_RESPONSE_MAX_BYTES).await?;
                let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
                    ProviderError::new(
                        ProviderErrorKind::Malformed,
                        format!("ollama /api/embed body: {e}"),
                    )
                })?;
                parse_embed_response(&req, &value)
            };
            match tokio::time::timeout(std::time::Duration::from_millis(bound_ms), call).await {
                Ok(result) => result,
                Err(_) => Err(embed_deadline_error(&req, bound_ms)),
            }
        })
    }

    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        // Adapter-boundary media gate (defense in depth alongside the
        // agent-side `CapabilityValidator`): the adapter's OWN capability
        // data decides, BEFORE any wire byte and before the body is built —
        // a vision-less model refuses every resolved image part with a typed
        // `BadRequest`, and a directly-constructed or hostile request can
        // never smuggle an image onto the native `images` array (or have it
        // silently dropped). The check also enforces the mime allowlist and
        // this adapter's per-image byte bound.
        let caps = self.capabilities(&req.model);
        if let Err(e) =
            faktor_provider::validate_media_delivery(&req, &caps, self.max_image_bytes())
        {
            return faktor_provider::provider_error_stream(e);
        }
        // The same gate for DOCUMENTS, mirroring the image gate: the native
        // wire has no document field, so any resolved `FileData` part is a
        // typed `BadRequest` BEFORE the body is built and before any wire
        // byte — a directly-constructed hostile request can never have a
        // document silently dropped by the lowering.
        if let Err(e) = faktor_provider::validate_document_delivery(
            &req,
            self.document_capable(&req.model),
            self.max_document_bytes(),
        ) {
            return faktor_provider::provider_error_stream(e);
        }
        let body = self.wire_body(&req);
        let url = format!("{}/api/chat", self.config.base_url);
        let transport = self.transport.clone();
        // One provider response per stream call: the response sequence
        // increments monotonically so tool ids (ollama:<seq>:<idx>) never
        // collide across responses of the same provider instance.
        let response_seq = self.response_seq.fetch_add(1, Ordering::Relaxed);
        let deadlines = stream_deadlines(&req);
        let cancel = req.meta.cancellation.clone();
        Box::pin(ollama_chat_stream(
            transport,
            url,
            body,
            response_seq,
            deadlines,
            Some(cancel),
        ))
    }
}

pub(crate) fn ollama_chat_stream(
    transport: Arc<dyn HttpTransport>,
    url: String,
    body: serde_json::Value,
    response_seq: u64,
    deadlines: StreamDeadlines,
    cancel: Option<faktor_core::cancellation::CancellationToken>,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> {
    use futures::StreamExt as _;
    type LineStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;
    enum Stage {
        Fresh,
        Streaming {
            lines: LineStream,
            /// Chunks produced by ONE parsed frame, drained one per poll so
            /// a frame carrying N tool calls yields N ToolCall chunks in
            /// order (the old loop dropped every call after the first).
            pending: VecDeque<ProviderChunk>,
            /// Frame with `done: true` seen; emit Done once pending drains.
            finished: bool,
        },
        Done,
    }
    futures::stream::unfold(Stage::Fresh, move |stage| {
        let transport = transport.clone();
        let url = url.clone();
        let deadlines = deadlines;
        let cancel = cancel.clone();
        let body = body.clone();
        async move {
            let (mut lines, mut pending, mut finished) = match stage {
                Stage::Fresh => {
                    let resp = execute_post_json(
                        transport.as_ref(),
                        &url,
                        reqwest::header::HeaderMap::new(),
                        &body,
                    )
                    .await;
                    match resp {
                        Ok(r) => {
                            let status = r.status();
                            if !status.is_success() {
                                let text = r.text().await.unwrap_or_default();
                                return Some((
                                    Err(status_to_provider_error(status, text)),
                                    Stage::Done,
                                ));
                            }
                            let lines: LineStream = Box::pin(guarded_lines(
                                utf8_line_stream(r.bytes_stream(), MAX_LINE_BYTES),
                                deadlines,
                                cancel.clone(),
                            ));
                            (lines, VecDeque::new(), false)
                        }
                        Err(e) => {
                            return Some((Err(ProviderError::from(e)), Stage::Done));
                        }
                    }
                }
                Stage::Streaming {
                    lines,
                    pending,
                    finished,
                } => (lines, pending, finished),
                Stage::Done => return None,
            };

            loop {
                // Drain one frame's chunks before reading the next line.
                if let Some(chunk) = pending.pop_front() {
                    return Some((
                        Ok(chunk),
                        Stage::Streaming {
                            lines,
                            pending,
                            finished,
                        },
                    ));
                }
                if finished {
                    return Some((Ok(ProviderChunk::Done), Stage::Done));
                }
                let Some(next) = lines.next().await else {
                    return Some((Ok(ProviderChunk::Done), Stage::Done));
                };
                let line = match next {
                    Ok(l) => l,
                    Err(e) => return Some((Err(e), Stage::Done)),
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    return Some((
                        Err(ProviderError::new(
                            ProviderErrorKind::Malformed,
                            format!("bad ollama NDJSON line: {line:?}"),
                        )),
                        Stage::Done,
                    ));
                };
                let chunks = parse_ollama_frame(&value, response_seq);
                pending = VecDeque::from(chunks);
                // A `done: true` frame only terminates the stream when it
                // carried no chunk: a final frame can still hold tool_calls
                // (or content), and a hostile server may keep sending.
                if pending.is_empty() && value.get("done").and_then(|d| d.as_bool()) == Some(true) {
                    finished = true;
                }
            }
        }
    })
}

/// One native NDJSON frame -> 0..n chunks, in the order they belong to the
/// conversation: `message.thinking` (a Reasoning chunk) before `content`
/// (a Text chunk), then one ToolCall chunk per `tool_calls` entry in array
/// order. Tool ids: a provider-supplied `id` wins; otherwise
/// `ollama:<response_seq>:<tool_index>` — never a synthesized
/// `ollama_call_<name>` (the old ids collided across calls and responses).
/// The final `done: true` frame also carries the token counters
/// (`prompt_eval_count` / `eval_count`), which become a canonical
/// [`ProviderChunk::Usage`] frame emitted LAST (before the stream's Done) —
/// see the usage mapping notes at the bottom of this function.
fn parse_ollama_frame(value: &serde_json::Value, response_seq: u64) -> Vec<ProviderChunk> {
    let mut chunks: Vec<ProviderChunk> = Vec::new();
    if let Some(msg) = value.get("message") {
        if let Some(thinking) = msg.get("thinking").and_then(|t| t.as_str()) {
            if !thinking.is_empty() {
                chunks.push(ProviderChunk::Reasoning {
                    text: thinking.to_string(),
                });
            }
        }
        if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                chunks.push(ProviderChunk::Text {
                    text: text.to_string(),
                });
            }
        }
        if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
            for (idx, tc) in tool_calls.iter().enumerate() {
                let function = tc.get("function");
                let name = function
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let args = function
                    .and_then(|f| f.get("arguments"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .filter(|i| !i.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("ollama:{response_seq}:{idx}"));
                chunks.push(ProviderChunk::ToolCall {
                    id,
                    name: name.to_string(),
                    input: args,
                    complete: true,
                });
            }
        }
    }
    // Usage mapping (audit Phase-1 item C): the native /api/chat final
    // frame reports `prompt_eval_count` (prompt tokens evaluated) and
    // `eval_count` (generated tokens; thinking tokens ARE part of the
    // generation, so `output_tokens` already contains them and the
    // informational `reasoning_tokens` subset stays zero — the API reports
    // no separate thinking count, so reasoning can never be double-billed).
    // The API exposes NO cache split: when the daemon reuses a loaded KV
    // context the evaluated count is the uncached remainder, and without a
    // reported split the conservative correct category is uncached = the
    // reported count with zero cache lines (cache is never invented).
    // Counts are integers; a hostile wrong-typed/negative counter is
    // ignored (0) and yields no frame — unknown fields never panic.
    let prompt_eval_count = value
        .get("prompt_eval_count")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let eval_count = value
        .get("eval_count")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    if prompt_eval_count > 0 || eval_count > 0 {
        let usage = CanonicalUsage {
            uncached_input_tokens: prompt_eval_count,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: eval_count,
            reasoning_tokens: 0,
            reported_cost: None,
            request_id: None,
        };
        // Contract with the stream loop: the usage frame must come BEFORE
        // the synthetic Done, so it lands at the END of this frame's chunk
        // list (after any reasoning/text/tool chunks of the same frame).
        chunks.push(ProviderChunk::Usage(usage));
    }
    chunks
}

fn caps_from_show(model: &str, show: &ShowResponse) -> ModelCapabilities {
    let mut caps = ModelCapabilities::small_local();
    // Context length from model_info: real /api/show responses carry
    // architecture-prefixed keys ("qwen3.context_length", never a bare
    // "context_length") — see context_length_from_model_info.
    if let Some(info) = &show.model_info {
        if let Some(ctx) = context_length_from_model_info(info) {
            caps.context = ctx as usize;
        }
    }
    // Capability flags from the /api/show capabilities list.
    if let Some(caps_list) = &show.capabilities {
        let has = |name: &str| caps_list.iter().any(|c| c == name);
        caps.tools = has("tools");
        caps.vision = has("vision");
        // Real Ollama daemons have shipped both spellings of the embedding
        // capability ("embedding" in the current API, "embeddings" in older
        // builds); accept either rather than silently disabling /api/embed.
        caps.embeddings = has("embeddings") || has("embedding");
        caps.thinking = has("reasoning") || has("thinking");
        // A probed "reasoning" capability means the model accepts effort
        // LEVELS for the native think knob ("low"/"medium"/"high");
        // boolean-only thinking models never advertise it.
        caps.reasoning = has("reasoning");
    }
    // qwen3.8-family models advertise large contexts; never exceed the
    // model's own reported context.
    let _ = model;
    caps
}

/// Context length from a parsed `/api/show` `model_info` value.
///
/// Real responses carry architecture-prefixed GGUF metadata:
/// `{"general.architecture": "qwen3", "qwen3.context_length": 262144}`
/// (newer daemons also nest: `{"general": {"architecture": "qwen3"}}`).
/// Lookup order: read the architecture from the `general` section (nested
/// or dotted), then `<arch>.context_length` (dotted or nested); fall back
/// to the unique key ending in `.context_length` (or the bare
/// `context_length` as a last resort). Ambiguity — several context keys
/// with no declared architecture — is NOT guessed.
fn context_length_from_model_info(info: &serde_json::Value) -> Option<u64> {
    let arch = info
        .get("general")
        .and_then(|g| g.get("architecture"))
        .and_then(|a| a.as_str())
        .or_else(|| info.get("general.architecture").and_then(|a| a.as_str()));
    if let Some(arch) = arch {
        if let Some(ctx) = info
            .get(format!("{arch}.context_length"))
            .and_then(|c| c.as_u64())
        {
            return Some(ctx);
        }
        if let Some(ctx) = info
            .get(arch)
            .and_then(|section| section.get("context_length"))
            .and_then(|c| c.as_u64())
        {
            return Some(ctx);
        }
    }
    let mut candidates: Vec<u64> = Vec::new();
    if let serde_json::Value::Object(map) = info {
        for (k, v) in map {
            if k == "context_length" || k.ends_with(".context_length") {
                if let Some(ctx) = v.as_u64() {
                    candidates.push(ctx);
                }
            }
        }
    }
    if candidates.len() == 1 {
        Some(candidates[0])
    } else {
        None
    }
}

/// Allocated context for `model` from a parsed `GET /api/ps` body. Real
/// loaded-model entries report the allocated window under
/// `details.context_length` (never `size`/`size_vram` — those are bytes,
/// not tokens). Matching tolerates a missing tag: entry name equals the
/// model name or extends it as `name:<tag>`. Hostile bodies are loud
/// errors; an unloaded model is `Ok(None)`; a model entry without a
/// reported allocation is `Ok(None)`. Never panics.
fn ps_allocated_context(body: &serde_json::Value, model: &str) -> Result<Option<u64>, Error> {
    let Some(models) = body.get("models") else {
        return Err(Error::new(
            ErrorKind::Malformed,
            "ollama /api/ps body lacks a models array",
        ));
    };
    let Some(models) = models.as_array() else {
        return Err(Error::new(
            ErrorKind::Malformed,
            "ollama /api/ps models is not an array",
        ));
    };
    for entry in models {
        let Some(entry) = entry.as_object() else {
            return Err(Error::new(
                ErrorKind::Malformed,
                "ollama /api/ps entry is not an object",
            ));
        };
        let Some(name) = entry.get("name").and_then(|n| n.as_str()) else {
            return Err(Error::new(
                ErrorKind::Malformed,
                "ollama /api/ps entry lacks a string name",
            ));
        };
        let matches = name == model
            || name
                .strip_prefix(model)
                .is_some_and(|rest| rest.starts_with(':'));
        if !matches {
            continue;
        }
        let allocated = entry
            .get("details")
            .and_then(|d| d.get("context_length"))
            .and_then(|c| c.as_u64())
            .or_else(|| entry.get("context_length").and_then(|c| c.as_u64()));
        return Ok(allocated);
    }
    Ok(None)
}

/// One in-flight native Ollama message object being assembled. Content
/// parts of the same target role coalesce (assistant messages may carry
/// content + thinking + tool_calls at once); a part that targets a
/// different role (or a tool result) flushes it first, so the emitted
/// message order always mirrors the generic part order.
#[derive(Default)]
struct NativeMessage {
    role: &'static str,
    content: Vec<String>,
    thinking: Vec<String>,
    images: Vec<String>,
    tool_calls: Vec<serde_json::Value>,
}

impl NativeMessage {
    fn new(role: &'static str) -> Self {
        Self {
            role,
            ..Self::default()
        }
    }
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    }
}

fn flush_native(acc: &mut Option<NativeMessage>, out: &mut Vec<serde_json::Value>) {
    let Some(a) = acc.take() else { return };
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), serde_json::json!(a.role));
    obj.insert("content".into(), serde_json::json!(a.content.join("\n")));
    if !a.thinking.is_empty() {
        obj.insert("thinking".into(), serde_json::json!(a.thinking.join("\n")));
    }
    if !a.images.is_empty() {
        obj.insert("images".into(), serde_json::json!(a.images));
    }
    if !a.tool_calls.is_empty() {
        obj.insert("tool_calls".into(), serde_json::Value::Array(a.tool_calls));
    }
    out.push(serde_json::Value::Object(obj));
}

fn switch_role(
    acc: &mut Option<NativeMessage>,
    out: &mut Vec<serde_json::Value>,
    role: &'static str,
) {
    if acc.as_ref().is_some_and(|a| a.role != role) {
        flush_native(acc, out);
    }
    if acc.is_none() {
        *acc = Some(NativeMessage::new(role));
    }
}

/// Ollama's native /api/chat expects raw base64 in `images`. Internal
/// image parts may carry a `data:<mime>;base64,` URI — strip the prefix;
/// anything else passes through untouched.
fn native_image_payload(url: &str) -> String {
    let Some(rest) = url.strip_prefix("data:") else {
        return url.to_string();
    };
    match rest.split_once(',') {
        Some((_, payload)) => payload.to_string(),
        None => url.to_string(),
    }
}

/// Map a non-success HTTP status onto the typed provider error class the
/// retry policy consumes: 401/403 Auth, 429 RateLimited, 408/504 Timeout,
/// any 5xx Server (all retryable except Auth/BadRequest), everything else
/// BadRequest — terminal, so a hostile/refusing call is never hammered.
fn status_to_provider_error(status: reqwest::StatusCode, text: String) -> ProviderError {
    let kind = match status.as_u16() {
        401 | 403 => ProviderErrorKind::Auth,
        429 => ProviderErrorKind::RateLimited,
        408 | 504 => ProviderErrorKind::Timeout,
        500..=599 => ProviderErrorKind::Server,
        _ => ProviderErrorKind::BadRequest,
    };
    ProviderError::with_code(kind, status.as_u16().to_string(), text)
}

/// Re-apply the provider's embedding batch bounds to a directly-constructed
/// request (the fields are public, so the typed constructor is not the only
/// path to a hostile batch). Every violation is a terminal `BadRequest`
/// BEFORE any wire byte; the wire never carries an unbounded batch.
fn validate_embedding_batch(inputs: &[String]) -> Result<(), ProviderError> {
    use faktor_provider::{
        MAX_EMBEDDING_INPUTS, MAX_EMBEDDING_INPUT_BYTES, MAX_EMBEDDING_TOTAL_INPUT_BYTES,
    };
    if inputs.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::BadRequest,
            "ollama /api/embed requires at least one input",
        ));
    }
    if inputs.len() > MAX_EMBEDDING_INPUTS {
        return Err(ProviderError::new(
            ProviderErrorKind::BadRequest,
            format!(
                "ollama /api/embed batch carries {} inputs, over the cap of {MAX_EMBEDDING_INPUTS}",
                inputs.len()
            ),
        ));
    }
    let mut total: usize = 0;
    for input in inputs {
        if input.is_empty() {
            return Err(ProviderError::new(
                ProviderErrorKind::BadRequest,
                "ollama /api/embed input is empty",
            ));
        }
        if input.len() > MAX_EMBEDDING_INPUT_BYTES {
            return Err(ProviderError::new(
                ProviderErrorKind::BadRequest,
                format!(
                    "ollama /api/embed input of {} bytes exceeds MAX_EMBEDDING_INPUT_BYTES ({MAX_EMBEDDING_INPUT_BYTES})",
                    input.len()
                ),
            ));
        }
        total = total.saturating_add(input.len());
    }
    if total > MAX_EMBEDDING_TOTAL_INPUT_BYTES {
        return Err(ProviderError::new(
            ProviderErrorKind::BadRequest,
            format!(
                "ollama /api/embed inputs total {total} bytes, over the cap of {MAX_EMBEDDING_TOTAL_INPUT_BYTES}"
            ),
        ));
    }
    Ok(())
}

/// Read a response body with a hard byte cap: a hostile daemon can never
/// stream an unbounded body into memory. Over the cap is a typed
/// `Malformed` refusal (retrying the same hostile body can never help).
async fn read_body_bounded(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| ProviderError::new(ProviderErrorKind::Network, format!("ollama body: {e}")))?
    {
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(ProviderError::new(
                ProviderErrorKind::Malformed,
                format!("ollama /api/embed response exceeds the {cap}-byte bound"),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Lower one `/api/embed` body into a validated [`EmbeddingResponse`]:
/// exactly one array per input, uniform non-zero dimension, every component
/// finite and within [`faktor_provider::MAX_EMBEDDING_DIMENSIONS`]. Every
/// hostile shape (missing/mistyped array, ragged batch, wrong count, NaN /
/// infinite component, oversized dimension) is a typed `Malformed` refusal
/// — never a panic and never a silently truncated vector list.
fn parse_embed_response(
    req: &EmbeddingRequest,
    value: &serde_json::Value,
) -> Result<EmbeddingResponse, ProviderError> {
    let Some(list) = value.get("embeddings").and_then(|e| e.as_array()) else {
        return Err(ProviderError::new(
            ProviderErrorKind::Malformed,
            "ollama /api/embed response lacks an embeddings array",
        ));
    };
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        let Some(array) = item.as_array() else {
            return Err(ProviderError::new(
                ProviderErrorKind::Malformed,
                format!("ollama /api/embed entry {index} is not an array"),
            ));
        };
        let mut vector: Vec<f32> = Vec::with_capacity(array.len());
        for component in array {
            let Some(value) = component.as_f64() else {
                return Err(ProviderError::new(
                    ProviderErrorKind::Malformed,
                    format!("ollama /api/embed entry {index} carries a non-numeric component"),
                ));
            };
            vector.push(value as f32);
        }
        vectors.push(vector);
    }
    let response = EmbeddingResponse::new(vectors)?;
    response.validate_for(req.inputs.len())?;
    Ok(response)
}

/// The deadline exceeded error, carrying the operation lineage from
/// [`EmbeddingRequest::meta`] so a timeout is attributable to the exact
/// operation/session that set it.
fn embed_deadline_error(req: &EmbeddingRequest, bound_ms: u64) -> ProviderError {
    let lineage = match &req.meta {
        Some(meta) => format!(
            " (operation {}, session {})",
            meta.operation_id, meta.session_id
        ),
        None => String::new(),
    };
    ProviderError::with_code(
        ProviderErrorKind::Timeout,
        "deadline",
        format!("ollama /api/embed exceeded its {bound_ms} ms bound{lineage}"),
    )
}

/// Drive one embedding future to completion from the SYNCHRONOUS
/// [`Provider::embed`] surface:
///
/// - inside a multi-threaded tokio runtime the future runs on the current
///   runtime under `block_in_place` (the daemon's worker keeps making
///   progress);
/// - anywhere else (current-thread runtimes, plain threads, tests) a
///   dedicated named thread drives a current-thread runtime, so the caller
///   never needs a reactor and the call site can never wedge another
///   runtime's driver.
///
/// A panic inside the future is caught and typed (a provider call never
/// aborts the caller). The call is bounded because every future passed here
/// wraps its whole body in a `timeout`.
fn run_embedding_future<F>(fut: F) -> Result<EmbeddingResponse, ProviderError>
where
    F: std::future::Future<Output = Result<EmbeddingResponse, ProviderError>> + Send + 'static,
{
    let panicked = |_| {
        Err(ProviderError::new(
            ProviderErrorKind::Malformed,
            "ollama /api/embed call panicked",
        ))
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tokio::task::block_in_place(|| handle.block_on(fut))
            }))
            .unwrap_or_else(panicked)
        }
        _ => {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("ollama-embed".into())
                .spawn(move || {
                    let outcome = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            rt.block_on(fut)
                        }))
                        .unwrap_or_else(|_| {
                            Err(ProviderError::new(
                                ProviderErrorKind::Malformed,
                                "ollama /api/embed call panicked",
                            ))
                        }),
                        Err(e) => Err(ProviderError::new(
                            ProviderErrorKind::Network,
                            format!("ollama embedding runtime unavailable: {e}"),
                        )),
                    };
                    let _ = tx.send(outcome);
                });
            match spawned {
                Ok(worker) => {
                    let outcome = rx.recv().unwrap_or_else(|_| {
                        Err(ProviderError::new(
                            ProviderErrorKind::Network,
                            "ollama embedding worker died before answering",
                        ))
                    });
                    let _ = worker.join();
                    outcome
                }
                Err(e) => Err(ProviderError::new(
                    ProviderErrorKind::Network,
                    format!("ollama embedding worker spawn failed: {e}"),
                )),
            }
        }
    }
}

/// Lower one generic request message into 0..n native Ollama message
/// objects, preserving part order:
///
/// - text parts become plain `content` on the declared role (user text ->
///   user content; assistant text -> assistant content);
/// - assistant reasoning parts become the message's native `thinking`
///   field; reasoning recorded under any other role is lifted onto a
///   role-assistant message (only assistant messages may carry thinking);
/// - `ToolCall` parts become native `{"function": {"name", "arguments"}}`
///   entries on an assistant message (`tool_calls` exist nowhere else);
///   their ids are remembered so results can name the call they answer;
/// - `ToolResult` parts become role-`tool` messages with the raw content,
///   plus the documented `tool_name` when the answered call was seen
///   earlier in this request. Ollama's native tool message has no
///   `is_error`/`tool_call_id` fields; content passes through verbatim.
fn lower_native_message(
    m: &RequestMessage,
    call_names: &mut HashMap<String, String>,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut acc: Option<NativeMessage> = None;
    for part in &m.content {
        match &part.kind {
            ContentKind::ToolResult { content, .. } => {
                flush_native(&mut acc, &mut out);
                let mut tm = serde_json::json!({ "role": "tool", "content": content });
                if let Some(cid) = part.tool_call_id.as_deref() {
                    if let Some(name) = call_names.get(cid) {
                        tm["tool_name"] = serde_json::json!(name);
                    }
                }
                out.push(tm);
            }
            ContentKind::ToolCall { id, name, input } => {
                switch_role(&mut acc, &mut out, "assistant");
                let a = acc.as_mut().expect("switch_role guarantees an accumulator");
                a.tool_calls.push(serde_json::json!({
                    "function": { "name": name, "arguments": input }
                }));
                if !id.is_empty() {
                    call_names.insert(id.clone(), name.clone());
                }
            }
            ContentKind::Reasoning { text } => {
                switch_role(&mut acc, &mut out, "assistant");
                let a = acc.as_mut().expect("switch_role guarantees an accumulator");
                a.thinking.push(text.clone());
            }
            ContentKind::Text { text } => {
                switch_role(&mut acc, &mut out, role_name(&m.role));
                let a = acc.as_mut().expect("switch_role guarantees an accumulator");
                a.content.push(text.clone());
            }
            ContentKind::Image { url } => {
                switch_role(&mut acc, &mut out, role_name(&m.role));
                let a = acc.as_mut().expect("switch_role guarantees an accumulator");
                a.images.push(native_image_payload(url));
            }
            ContentKind::ImageData { data, .. } => {
                // Resolved attachment bytes: native /api/chat takes raw
                // base64 (no data-URI prefix) — the same shape the legacy
                // URL path strips down to.
                switch_role(&mut acc, &mut out, role_name(&m.role));
                let a = acc.as_mut().expect("switch_role guarantees an accumulator");
                a.images.push(data.to_base64());
            }
            ContentKind::FileData { .. } => {
                // Documents are NOT deliverable on the native ollama wire:
                // `document_capable` stays false, the delivery gate refuses
                // every resolved document part typedly BEFORE this lowering
                // runs, and this arm can only be reached by a directly
                // constructed hostile request — which must never smuggle a
                // document into `images` or any other field.
            }
        }
    }
    flush_native(&mut acc, &mut out);
    out
}

/// Native /api/chat thinking knob driven by the request's stored
/// ReasoningMode: `Off` -> `think: false`; `Low`/`Medium`/`High` -> the
/// documented effort level string when the model profile indicates
/// effort-level reasoning (`ModelCapabilities::reasoning`), else the
/// boolean knob for boolean-thinking models (`ModelCapabilities::thinking`).
/// A profile supporting neither leaves the knob absent (server default).
fn thinking_knob(mode: ReasoningMode, caps: &ModelCapabilities) -> Option<serde_json::Value> {
    match mode {
        ReasoningMode::Off => Some(serde_json::json!(false)),
        ReasoningMode::Low => effort_or_boolean_knob("low", caps),
        ReasoningMode::Medium => effort_or_boolean_knob("medium", caps),
        ReasoningMode::High => effort_or_boolean_knob("high", caps),
    }
}

fn effort_or_boolean_knob(effort: &str, caps: &ModelCapabilities) -> Option<serde_json::Value> {
    if caps.reasoning {
        Some(serde_json::json!(effort))
    } else if caps.thinking {
        Some(serde_json::json!(true))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::id::{OpId, SessionId};
    use faktor_provider::egress::{HttpTransport, MockHttpTransport};
    use faktor_provider::testing::{MockAction, MockServer};
    use faktor_provider::{ContentPart, RequestMessage, RequestMeta, ToolSpec};
    use faktor_security::destination::DestinationPolicy;
    use futures::StreamExt;

    fn req(model: &str) -> GenericAgentRequest {
        GenericAgentRequest {
            model: model.into(),
            system: "sys".into(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec![ContentPart::text("hi")],
            }],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_output: Some(256),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: OpId::new(1),
                session_id: SessionId::new(1),
                provider: "ollama".into(),
                attempt: 0,
                deadline_ms: 5000,
                cancellation: CancellationToken::new(),
            },
        }
    }

    /// Drain a provider stream, panicking on any error (tests that assert
    /// error paths collect the chunks inline instead).
    async fn stream_chunks(
        provider: &dyn Provider,
        request: GenericAgentRequest,
    ) -> Vec<ProviderChunk> {
        let mut stream = provider.stream(request);
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.push(chunk.unwrap_or_else(|e| panic!("unexpected provider error: {e:?}")));
        }
        out
    }

    #[tokio::test]
    async fn discovery_via_api_tags() {
        let server = MockServer::new();
        server.route(
            "GET",
            "/api/tags",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"qwen3.8:latest"},{"name":"llama3.2:3b"}]}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        let models = provider.discover_models().await.unwrap();
        assert_eq!(models, vec!["llama3.2:3b", "qwen3.8:latest"]);
    }

    #[tokio::test]
    async fn discovery_failure_is_loud() {
        let provider = OllamaProvider::new(OllamaConfig::new(Some("http://127.0.0.1:1".into())));
        assert!(provider.discover_models().await.is_err());
    }

    #[tokio::test]
    async fn capability_probe_maps_metadata() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/show",
            MockAction::Respond {
                status: 200,
                // Real /api/show metadata: architecture-prefixed keys, not a
                // bare "context_length".
                body: r#"{
                    "model_info": {
                        "general.architecture": "qwen3",
                        "qwen3.context_length": 262144
                    },
                    "capabilities": ["tools", "vision", "embeddings", "reasoning"]
                }"#
                .into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        let caps = provider.probe_model("qwen3.8").await.unwrap();
        assert_eq!(caps.context, 262_144);
        assert!(caps.tools);
        assert!(caps.vision);
        assert!(caps.embeddings);
        assert!(caps.thinking);
        assert!(caps.reasoning, "a reasoning capability means effort levels");
    }

    #[tokio::test]
    async fn wire_shape_is_native_and_clean() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::AssertThenRespond {
                status: 200,
                body: r#"{"message":{"role":"assistant","content":"pong"},"done":true}"#.into(),
                assert: Arc::new(|body: &serde_json::Value| {
                    assert_eq!(body["model"], "qwen3.8");
                    assert!(body["stream"].as_bool().unwrap());
                    // Native keep_alive is present.
                    assert!(body["keep_alive"].is_string());
                    // Tools in native shape.
                    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
                    // Messages are native objects: role + plain-string
                    // content — never OpenAI-style typed content arrays.
                    assert_eq!(
                        body["messages"],
                        serde_json::json!([
                            { "role": "system", "content": "sys" },
                            { "role": "user", "content": "hi" },
                        ])
                    );
                    // Internal fields never leak.
                    for leaked in [
                        "operation_id",
                        "session_id",
                        "attempt",
                        "deadline_ms",
                        "cancellation",
                    ] {
                        assert!(
                            !body.as_object().unwrap().contains_key(leaked),
                            "{leaked} leaked!"
                        );
                    }
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut stream = provider.stream(req("qwen3.8"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "pong");
    }

    #[tokio::test]
    async fn request_meta_deadline_bounds_silent_stream() {
        // Audit round 15: `RequestMeta::deadline_ms` is the operation
        // deadline. A silent daemon with meta.deadline_ms = 1200 must error
        // Timeout at the overall bound (~1.2s, well inside 2.5s) instead of
        // waiting out the 60s first-byte default.
        let server = MockServer::new();
        server.route("POST", "/api/chat", MockAction::Silent { status: 200 });
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut g = req("qwen3.8");
        g.meta.deadline_ms = 1200;
        let mut stream = provider.stream(g);
        let t0 = std::time::Instant::now();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("the meta deadline must terminate the silent stream")
            .expect("an error item");
        let err = item.expect_err("must be a timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(2500),
            "meta deadline must fire at its overall bound: {:?}",
            t0.elapsed()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("the stream must end after the terminal error")
                .is_none(),
            "no further events after the meta-deadline timeout"
        );
    }

    #[tokio::test]
    async fn native_tool_call_parsed() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a.rs"}}}]},"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut stream = provider.stream(req("qwen3.8"));
        let mut call = None;
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::ToolCall { name, input, .. } => call = Some((name, input)),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        let (name, input) = call.expect("tool call");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "a.rs");
    }

    #[tokio::test]
    async fn malformed_ndjson_is_malformed_error() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: "{\"message\": {\"content\": \"partial\"}}\n{not json\n".into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut stream = provider.stream(req("qwen3.8"));
        let mut saw_error = false;
        let mut got_partial = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ProviderChunk::Text { .. }) => got_partial = true,
                Err(e) if e.kind == ProviderErrorKind::Malformed => saw_error = true,
                Ok(ProviderChunk::Done) => break,
                _ => {}
            }
        }
        assert!(got_partial);
        assert!(saw_error, "garbage NDJSON must be a loud error");
    }

    #[tokio::test]
    async fn rate_limit_mapped() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 429,
                body: r#"{"error":"rate limited"}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut stream = provider.stream(req("qwen3.8"));
        let err = stream.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
        assert!(err.retryable);
    }

    #[tokio::test]
    async fn missing_model_is_not_found_via_probe() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/show",
            MockAction::Respond {
                status: 404,
                body: r#"{"error":"model not found"}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        let err = provider.probe_model("ghost").await.unwrap_err();
        assert!(err.kind == ErrorKind::NotFound, "{err:?}");
    }

    #[test]
    fn default_capabilities_are_small_local() {
        let provider = OllamaProvider::build(OllamaConfig::new(None));
        let caps = provider.capabilities("anything");
        assert_eq!(caps.context, 32_768);
        assert!(caps.tools);
        assert!(caps.embeddings);
    }

    #[tokio::test]
    async fn ndjson_frame_split_across_http_chunks_assembles() {
        // Ollama streams NDJSON; the old per-chunk .lines() corrupts a
        // frame split by HTTP chunking. Must reassemble.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![
                    b"{\"message\":{\"role\":\"assistant\",\"content\":\"par".to_vec(),
                    b"tial\"},\"done\":true}\n".to_vec(),
                    b"{\"message\":{\"role\":\"assistant\",\"content\":\" tail\"},\"done\":false}\n".to_vec(),
                    b"{\"done\":true}\n".to_vec(),
                ],
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut stream = provider.stream(req("qwen3.8"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ProviderChunk::Text { text: t }) => text.push_str(&t),
                Ok(ProviderChunk::Done) => break,
                Ok(_) => {}
                Err(e) => panic!("fragmented NDJSON must assemble, got {e:?}"),
            }
        }
        assert_eq!(text, "partial tail");
    }

    #[tokio::test]
    async fn refresh_from_live_drives_capabilities() {
        // Spec §10: discovery/probing drive behavior — after warm-up,
        // capabilities() reflects the LIVE probed model, not the default
        // small-local constant. (The audit: probe APIs existed but nothing
        // ever called them.)
        let server = MockServer::new();
        server.route(
            "GET",
            "/api/tags",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"qwen3.8:latest"},{"name":"dead-server-model"}]}"#
                    .into(),
            },
        );
        server.route(
            "POST",
            "/api/show",
            MockAction::Respond {
                status: 200,
                // Real /api/show metadata (architecture-prefixed keys).
                body: r#"{
                    "model_info": {
                        "general.architecture": "qwen3",
                        "qwen3.context_length": 262144
                    },
                    "capabilities": ["tools", "vision", "reasoning"]
                }"#
                .into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        // Before warm-up: the conservative default.
        assert_eq!(
            provider.capabilities("qwen3.8:latest").context,
            ModelCapabilities::small_local().context,
            "pre-warm default is the conservative profile"
        );
        let n = provider.refresh_from_live().await.unwrap();
        assert_eq!(n, 2, "both discovered models probed");
        let caps = provider.capabilities("qwen3.8:latest");
        assert_eq!(caps.context, 262_144);
        assert!(caps.tools);
        assert!(caps.vision);
        assert!(caps.thinking);
        // Unknown models stay on the default.
        assert_eq!(
            provider.capabilities("not-installed").context,
            ModelCapabilities::small_local().context
        );
    }

    #[tokio::test]
    async fn refresh_survives_hostile_tags_body() {
        // A garbage /api/tags response must surface as an error, and a
        // dead probe target must not poison the cache.
        let server = MockServer::new();
        server.route(
            "GET",
            "/api/tags",
            MockAction::Respond {
                status: 200,
                body: "{not json".into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        assert!(provider.refresh_from_live().await.is_err());
        assert_eq!(
            provider.capabilities("qwen3.8:latest").context,
            ModelCapabilities::small_local().context
        );
    }

    #[tokio::test]
    async fn native_wire_frame_shape() {
        // P0: the generic request must lower to REAL Ollama message objects
        // (role + plain-string content) — never OpenAI-style content arrays.
        // A request with system + user text and NO images must not carry an
        // images field at all.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::AssertThenRespond {
                status: 200,
                body: r#"{"message":{"role":"assistant","content":"ok"},"done":true}"#.into(),
                assert: Arc::new(|body: &serde_json::Value| {
                    assert_eq!(
                        body["messages"],
                        serde_json::json!([
                            { "role": "system", "content": "sys" },
                            { "role": "user", "content": "hi" },
                        ]),
                        "system is mapped to the first message; user text is a plain content string"
                    );
                    for msg in body["messages"].as_array().unwrap() {
                        for key in msg.as_object().unwrap().keys() {
                            assert!(
                                ["role", "content", "thinking", "images", "tool_calls"]
                                    .contains(&key.as_str()),
                                "message carries a non-native field: {key}"
                            );
                        }
                    }
                    let user_msg = &body["messages"][1];
                    assert!(
                        !user_msg.as_object().unwrap().contains_key("images"),
                        "no images in the request -> the images field is omitted"
                    );
                    assert!(
                        !serde_json::to_string(&body["messages"])
                            .unwrap()
                            .contains("\"type\""),
                        "typed content blocks must never reach the Ollama wire"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let chunks = stream_chunks(&*provider, req("qwen3.8")).await;
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
    }

    #[tokio::test]
    async fn images_lower_to_native_base64_array() {
        // Content parts of kind image become the native `images` array
        // (raw base64 — a data: URI prefix is stripped), never an OpenAI
        // image_url block.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut r = req("qwen3.8");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("what is this?"),
                ContentPart {
                    kind: ContentKind::Image {
                        url: "data:image/png;base64,QUJD".into(),
                    },
                    tool_call_id: None,
                },
            ],
        });
        let chunks = stream_chunks(&*provider, r).await;
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
        let (_, _, raw) = server.last_request().unwrap();
        let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let msg = &body["messages"][2];
        assert_eq!(
            msg["content"], "what is this?",
            "text survives alongside images"
        );
        assert_eq!(
            msg["images"],
            serde_json::json!(["QUJD"]),
            "data-URI prefix stripped to base64"
        );
        assert_eq!(
            body["messages"][1]["images"],
            serde_json::Value::Null,
            "image-less messages omit images"
        );
    }

    #[tokio::test]
    async fn resolved_image_data_lowers_to_native_base64_array() {
        // Resolved `ImageData` parts lower to the SAME native `images`
        // array as a data-URI URL: raw standard base64, no prefix. The model
        // must advertise vision (the adapter-boundary gate refuses a
        // resolved image for a vision-less profile before lowering).
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
        let expected = faktor_provider::MediaBytes::new(png.clone())
            .unwrap()
            .to_base64();
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let mut cfg = OllamaConfig::new(Some(base));
        cfg.model_overrides.insert(
            "qwen3.8".into(),
            ModelCapabilities {
                vision: true,
                ..ModelCapabilities::default()
            },
        );
        let provider = OllamaProvider::build(cfg);
        let mut r = req("qwen3.8");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("what is this?"),
                ContentPart::image_data("image/png", png).unwrap(),
            ],
        });
        let chunks = stream_chunks(&*provider, r).await;
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
        let (_, _, raw) = server.last_request().unwrap();
        let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let msg = &body["messages"][2];
        assert_eq!(msg["content"], "what is this?");
        assert_eq!(
            msg["images"],
            serde_json::json!([expected]),
            "resolved bytes lower byte-exactly to raw base64"
        );
    }

    #[tokio::test]
    async fn adapter_media_gate_refuses_images_when_the_model_lacks_vision() {
        // Adapter-boundary gate: the adapter's OWN capability data (the
        // probed `vision` flag) decides, alongside the agent-side
        // CapabilityValidator. A vision-less model + a resolved image part
        // is a typed, single-frame BadRequest — and NO wire byte is sent
        // (the mock server must record zero requests).
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        // Capability data explicitly probed as vision-less.
        assert!(!provider.capabilities("qwen3.8").vision);
        let mut r = req("qwen3.8");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G', 1]).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        let item = stream
            .next()
            .await
            .expect("the refusal is a single terminal frame");
        let err = item.expect_err("a vision-less model must refuse the image");
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(!err.retryable, "nothing was attempted; retry cannot help");
        assert!(
            err.message.contains("does not support vision"),
            "typed refusal names the capability: {err:?}"
        );
        assert!(
            stream.next().await.is_none(),
            "the refusal stream must terminate"
        );
        assert_eq!(
            server.request_count(),
            0,
            "the gate runs BEFORE any wire byte is sent"
        );
        // The legacy URL-shaped image part is not bytes-bearing and never
        // reaches this gate; only resolved ImageData is admitted/refused.
        assert_eq!(provider.max_image_bytes(), OLLAMA_MAX_IMAGE_BYTES);
    }

    #[tokio::test]
    async fn adapter_document_gate_refuses_documents_typed_before_the_wire() {
        // Adapter-boundary DOCUMENT gate (parity with the image gate): the
        // native /api/chat wire has no document field, so a resolved
        // FileData part is a typed, single-frame BadRequest and NO wire byte
        // is sent. The model is explicitly vision-capable so the refusal can
        // only come from the document gate.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let mut cfg = OllamaConfig::new(Some(base));
        cfg.model_overrides.insert(
            "qwen3.8".into(),
            ModelCapabilities {
                vision: true,
                ..ModelCapabilities::default()
            },
        );
        let provider = OllamaProvider::new(cfg);
        assert!(
            !provider.document_capable("qwen3.8"),
            "the native wire has no document field"
        );
        let mut r = req("qwen3.8");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart::file_data(
                "application/pdf",
                Some("spec.pdf"),
                b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec(),
            )
            .unwrap()],
        });
        let mut stream = provider.stream(r);
        let item = stream
            .next()
            .await
            .expect("the refusal is a single terminal frame");
        let err = item.expect_err("ollama must refuse a resolved document part");
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(!err.retryable, "nothing was attempted; retry cannot help");
        assert!(
            err.message.contains("does not support document input"),
            "typed refusal names the capability: {err:?}"
        );
        assert!(
            stream.next().await.is_none(),
            "the refusal stream must terminate"
        );
        assert_eq!(
            server.request_count(),
            0,
            "the document gate runs BEFORE any wire byte is sent"
        );
    }

    #[tokio::test]
    async fn adapter_media_gate_lowers_images_for_vision_models() {
        // A vision-capable model (the adapter's own probed data) admits the
        // same request and the image lowers byte-exactly onto the native
        // `images` array.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::AssertThenRespond {
                status: 200,
                body: r#"{"done":true}"#.into(),
                assert: Arc::new(|body: &serde_json::Value| {
                    assert!(
                        body["messages"][2]["images"]
                            .as_array()
                            .is_some_and(|a| !a.is_empty()),
                        "a vision model must receive the lowered image: {body}"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let mut cfg = OllamaConfig::new(Some(base));
        cfg.model_overrides.insert(
            "qwen3.8".into(),
            ModelCapabilities {
                vision: true,
                ..ModelCapabilities::default()
            },
        );
        let provider = OllamaProvider::new(cfg);
        assert!(
            provider.capabilities("qwen3.8").vision,
            "adapter capability data admits vision"
        );
        let mut r = req("qwen3.8");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G', 9, 9]).unwrap(),
            ],
        });
        let chunks = stream_chunks(&*provider, r).await;
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
        assert!(
            server.request_count() >= 1,
            "the vision model request reached the wire"
        );
    }

    #[tokio::test]
    async fn adapter_media_gate_refuses_hostile_oversized_payload_typed() {
        // Hostile, maximally-sized payload: a valid-PNG-magic image exactly
        // one byte over the adapter's per-image bound. The refusal is typed
        // (`BadRequest`, not a panic, not a truncated send, not an OOM) and
        // happens before any wire byte.
        let png = [0x89, b'P', b'N', b'G'];
        let oversized: Vec<u8> = png
            .iter()
            .copied()
            .chain(std::iter::repeat_n(
                0u8,
                OLLAMA_MAX_IMAGE_BYTES + 1 - png.len(),
            ))
            .collect();
        assert_eq!(oversized.len(), OLLAMA_MAX_IMAGE_BYTES + 1);
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let mut cfg = OllamaConfig::new(Some(base));
        cfg.model_overrides.insert(
            "qwen3.8".into(),
            ModelCapabilities {
                vision: true,
                ..ModelCapabilities::default()
            },
        );
        let provider = OllamaProvider::new(cfg);
        let mut r = req("qwen3.8");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart::image_data("image/png", oversized).unwrap()],
        });
        let mut stream = provider.stream(r);
        let item = stream.next().await.expect("one typed refusal frame");
        let err = item.expect_err("the oversized payload must be refused");
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(
            err.message.contains("exceeds the provider bound"),
            "typed bound refusal: {err:?}"
        );
        assert!(
            stream.next().await.is_none(),
            "the refusal stream must terminate"
        );
        assert_eq!(
            server.request_count(),
            0,
            "an oversized image never starts a request"
        );
    }

    /// Stream one /api/chat request against a scratch mock server and
    /// return the recorded request body (asserting the stream completed).
    async fn chat_request_body(
        reasoning: Option<ReasoningMode>,
        effort_capable: bool,
    ) -> serde_json::Value {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let mut cfg = OllamaConfig::new(Some(base));
        if effort_capable {
            cfg.model_overrides.insert(
                "qwen3.8".into(),
                ModelCapabilities {
                    reasoning: true,
                    thinking: true,
                    ..ModelCapabilities::default()
                },
            );
        }
        let provider = OllamaProvider::new(cfg);
        let mut r = req("qwen3.8");
        r.reasoning = reasoning;
        let chunks = stream_chunks(&*provider, r).await;
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
        let (_, _, raw) = server.last_request().unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[tokio::test]
    async fn thinking_request_body() {
        // The request's stored ReasoningMode drives the native /api/chat
        // `think` knob — never guessed from content text.
        let off = chat_request_body(Some(ReasoningMode::Off), false).await;
        assert_eq!(off["think"], serde_json::json!(false), "Off -> think:false");
        let bool_knob = chat_request_body(Some(ReasoningMode::Medium), false).await;
        assert_eq!(
            bool_knob["think"],
            serde_json::json!(true),
            "boolean-thinking profile -> think:true for any non-Off level"
        );
        let effort = chat_request_body(Some(ReasoningMode::High), true).await;
        assert_eq!(
            effort["think"],
            serde_json::json!("high"),
            "effort-capable profile -> documented level string"
        );
        let low = chat_request_body(Some(ReasoningMode::Low), true).await;
        assert_eq!(low["think"], serde_json::json!("low"));
        let none = chat_request_body(None, false).await;
        assert!(
            !none.as_object().unwrap().contains_key("think"),
            "no reasoning mode -> knob omitted (server default)"
        );
    }

    #[tokio::test]
    async fn thinking_response_parsed() {
        // Native /api/chat surfaces thinking as message.thinking. A frame
        // carrying thinking AND content must yield the Reasoning chunk
        // before the Text chunk.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"message":{"role":"assistant","thinking":"let me reason","content":"the answer"},"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let chunks = stream_chunks(&*provider, req("qwen3.8")).await;
        let mut kinds = Vec::new();
        for chunk in &chunks {
            match chunk {
                ProviderChunk::Reasoning { text } => {
                    assert_eq!(text, "let me reason");
                    kinds.push("reasoning");
                }
                ProviderChunk::Text { text } => {
                    assert_eq!(text, "the answer");
                    kinds.push("text");
                }
                ProviderChunk::Done => kinds.push("done"),
                other => panic!("unexpected chunk {other:?}"),
            }
        }
        assert_eq!(
            kinds,
            ["reasoning", "text", "done"],
            "thinking precedes text"
        );
    }

    #[tokio::test]
    async fn multiple_tool_calls_one_frame() {
        // ONE native frame with N tool_calls must yield N ToolCall chunks
        // in array order — the old parser returned after the first call
        // and silently dropped the rest.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a.rs"}}},{"function":{"name":"write_file","arguments":{"path":"b.txt","content":"hi"}}},{"function":{"name":"read_file","arguments":{"path":"c.rs"}}}]},"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let chunks = stream_chunks(&*provider, req("qwen3.8")).await;
        let calls: Vec<(String, String, serde_json::Value, bool)> = chunks
            .iter()
            .filter_map(|c| match c {
                ProviderChunk::ToolCall {
                    id,
                    name,
                    input,
                    complete,
                } => Some((id.clone(), name.clone(), input.clone(), *complete)),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 3, "all three calls must surface as chunks");
        assert_eq!(calls[0].0, "ollama:0:0");
        assert_eq!(calls[0].1, "read_file");
        assert_eq!(calls[0].2["path"], "a.rs");
        assert_eq!(calls[1].0, "ollama:0:1");
        assert_eq!(calls[1].1, "write_file");
        assert_eq!(calls[1].2["content"], "hi");
        assert_eq!(calls[2].0, "ollama:0:2");
        assert_eq!(calls[2].1, "read_file");
        assert_eq!(calls[2].2["path"], "c.rs");
        assert!(
            calls.iter().all(|(_, _, _, complete)| *complete),
            "a native tool call arrives complete"
        );
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
    }

    #[tokio::test]
    async fn tool_id_uniqueness() {
        // Ids are ollama:<response-seq>:<tool-index>: two calls of the same
        // tool inside ONE response get distinct ids, and ids from a second
        // response never collide with the first. Never ollama_call_<name>.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Sequence {
                actions: vec![
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"a.rs"}}},{"function":{"name":"read_file","arguments":{"path":"b.rs"}}}]},"done":true}"#.into(),
                    },
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"c.rs"}}}]},"done":true}"#.into(),
                    },
                ],
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        let ids: Vec<String> = stream_chunks(&*provider, req("qwen3.8"))
            .await
            .into_iter()
            .filter_map(|c| match c {
                ProviderChunk::ToolCall { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        let ids2: Vec<String> = stream_chunks(&*provider, req("qwen3.8"))
            .await
            .into_iter()
            .filter_map(|c| match c {
                ProviderChunk::ToolCall { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["ollama:0:0".to_string(), "ollama:0:1".to_string()]
        );
        assert_eq!(ids2, vec!["ollama:1:0".to_string()]);
        let mut all = ids.clone();
        all.extend(ids2.iter().cloned());
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            all.len(),
            "ids must be unique across responses"
        );
        assert!(
            ids.iter()
                .chain(ids2.iter())
                .all(|id| id.starts_with("ollama:") && !id.contains("ollama_call")),
            "ids are ollama:<seq>:<idx>, never synthesized name ids"
        );
    }

    #[tokio::test]
    async fn tool_result_lowering() {
        // A request replaying a finished tool round trip serializes as the
        // native form: assistant tool_calls, then role-"tool" messages with
        // the raw content and the documented tool_name when the answered
        // call is visible in this request (an orphan result is sent without
        // a tool_name, never fabricated).
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::build(OllamaConfig::new(Some(base)));
        let mut r = req("qwen3.8");
        r.messages.push(RequestMessage {
            role: Role::Assistant,
            content: vec![ContentPart::tool_call(
                "call_1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )],
        });
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::tool_result("fn main() {}", false, "call_1"),
                ContentPart::tool_result("boom", true, "call_missing"),
            ],
        });
        let chunks = stream_chunks(&*provider, r).await;
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
        let (_, _, raw) = server.last_request().unwrap();
        let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            body["messages"],
            serde_json::json!([
                { "role": "system", "content": "sys" },
                { "role": "user", "content": "hi" },
                {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": "read_file", "arguments": { "path": "a.rs" } }
                    }]
                },
                { "role": "tool", "content": "fn main() {}", "tool_name": "read_file" },
                { "role": "tool", "content": "boom" }
            ])
        );
    }

    #[tokio::test]
    async fn architecture_prefixed_context() {
        // Real /api/show reports "<arch>.context_length" (never a bare
        // "context_length"); the architecture comes from the general
        // section, nested or dotted.
        async fn probe(body: &str) -> ModelCapabilities {
            let server = MockServer::new();
            server.route(
                "POST",
                "/api/show",
                MockAction::Respond {
                    status: 200,
                    body: body.into(),
                },
            );
            let base = server.base_url().await;
            let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
            provider.probe_model("qwen3.8").await.unwrap()
        }
        // Nested general section + dotted arch key (field-report fixture).
        let nested = probe(
            r#"{"model_info":{"general":{"architecture":"qwen3"},"qwen3.context_length":262144}}"#,
        )
        .await;
        assert_eq!(nested.context, 262_144);
        // Flat real metadata.
        let flat = probe(
            r#"{"model_info":{"general.architecture":"gemma3","gemma3.context_length":131072}}"#,
        )
        .await;
        assert_eq!(flat.context, 131_072);
        // Fallback: no architecture declared, exactly one ".context_length".
        let fallback = probe(r#"{"model_info":{"llama.context_length":131072}}"#).await;
        assert_eq!(fallback.context, 131_072);
        // Hostile: several context keys with no declared architecture are
        // NOT guessed (a wrong guess silently truncates conversations).
        let ambiguous = probe(
            r#"{"model_info":{"llama.context_length":131072,"qwen3.context_length":262144}}"#,
        )
        .await;
        assert_eq!(ambiguous.context, ModelCapabilities::small_local().context);
        // Hostile: architecture declared but no matching context key.
        let missing = probe(r#"{"model_info":{"general.architecture":"qwen3"}}"#).await;
        assert_eq!(missing.context, ModelCapabilities::small_local().context);
    }

    #[tokio::test]
    async fn runtime_context_min() {
        // /api/ps reports the window ALLOCATED in the loaded process; the
        // effective context is min(model max from /api/show, allocation).
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/show",
            MockAction::Respond {
                status: 200,
                body: r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":262144}}"#.into(),
            },
        );
        server.route(
            "GET",
            "/api/ps",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"qwen3.8:latest","size_vram":42,"details":{"context_length":8192}}]}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        let ctx = provider.runtime_context("qwen3.8").await.unwrap();
        assert_eq!(ctx.model_max, 262_144, "model max comes from /api/show");
        assert_eq!(
            ctx.runtime_effective,
            Some(8192),
            "runtime effective is the smaller ps allocation"
        );
        assert_eq!(
            provider.effective_context("qwen3.8").await.unwrap(),
            Some(8192)
        );
    }

    #[tokio::test]
    async fn runtime_context_clamps_allocation_above_model_max() {
        // A ps allocation above the model's own maximum is clamped: the
        // model can never serve more than its declared context.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/show",
            MockAction::Respond {
                status: 200,
                body: r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":262144}}"#.into(),
            },
        );
        server.route(
            "GET",
            "/api/ps",
            MockAction::Respond {
                status: 200,
                body:
                    r#"{"models":[{"name":"qwen3.8:latest","details":{"context_length":524288}}]}"#
                        .into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        assert_eq!(
            provider.effective_context("qwen3.8").await.unwrap(),
            Some(262_144),
            "effective context never exceeds the model maximum"
        );
    }

    #[tokio::test]
    async fn effective_context_survives_hostile_ps() {
        async fn probe_with_ps(
            ps_status: u16,
            ps_body: &str,
        ) -> faktor_core::Result<Option<usize>> {
            let server = MockServer::new();
            server.route(
                "POST",
                "/api/show",
                MockAction::Respond {
                    status: 200,
                    body: r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":262144}}"#.into(),
                },
            );
            server.route(
                "GET",
                "/api/ps",
                MockAction::Respond {
                    status: ps_status,
                    body: ps_body.into(),
                },
            );
            let base = server.base_url().await;
            let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
            provider.effective_context("qwen3.8").await
        }
        // Garbage body: loud error, never a panic.
        let err = probe_with_ps(200, "{not json").await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed, "{err}");
        // Non-2xx: loud error.
        let err = probe_with_ps(500, "{}").await.unwrap_err();
        assert!(
            !err.retryable,
            "a dead /api/ps must not be blindly retried: {err}"
        );
        assert!(
            matches!(
                &err.kind,
                ErrorKind::Provider { code, retryable: false } if code == "500"
            ),
            "{err:?}"
        );
        // Model not loaded: Ok(None), the caller falls back to model max.
        let none = probe_with_ps(
            200,
            r#"{"models":[{"name":"llama3.2:latest","details":{"context_length":8192}}]}"#,
        )
        .await
        .unwrap();
        assert_eq!(none, None);
        // Hostile field types are treated as absent, not panics.
        let none = probe_with_ps(
            200,
            r#"{"models":[{"name":"qwen3.8:latest","details":{"context_length":"8192"}}]}"#,
        )
        .await
        .unwrap();
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn runtime_context_limit_is_the_cached_ps_min_of_model_max() {
        // P0 (the /api/ps allocation reaches the REAL budget): after a live
        // refresh, the SYNC runtime_context_limit serves the CACHED
        // min(model max from /api/show, /api/ps allocation) — a model that
        // advertises 256K but is loaded with a 64K window must report 64K,
        // never the advertised maximum.
        async fn provider_with_ps(ps_body: &str) -> (Arc<MockServer>, Arc<OllamaProvider>) {
            let server = MockServer::new();
            server.route(
                "GET",
                "/api/tags",
                MockAction::Respond {
                    status: 200,
                    body: r#"{"models":[{"name":"qwen3.8:latest"}]}"#.into(),
                },
            );
            server.route(
                "POST",
                "/api/show",
                MockAction::Respond {
                    status: 200,
                    body: r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":262144}}"#.into(),
                },
            );
            server.route(
                "GET",
                "/api/ps",
                MockAction::Respond {
                    status: 200,
                    body: ps_body.into(),
                },
            );
            let base = server.base_url().await;
            let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
            (server, provider)
        }
        // Allocated 64K of a 256K maximum: the limit is the allocation.
        let (server, provider) = provider_with_ps(
            r#"{"models":[{"name":"qwen3.8:latest","details":{"context_length":65536}}]}"#,
        )
        .await;
        assert_eq!(provider.refresh_from_live().await.unwrap(), 1);
        assert_eq!(
            provider.runtime_context_limit("qwen3.8:latest"),
            Some(65_536),
            "the runtime limit is min(model max, /api/ps allocation)"
        );
        // Allocation ABOVE the model maximum clamps to the model maximum
        // (a model can never serve more than its declared context).
        server.route(
            "GET",
            "/api/ps",
            MockAction::Respond {
                status: 200,
                body:
                    r#"{"models":[{"name":"qwen3.8:latest","details":{"context_length":524288}}]}"#
                        .into(),
            },
        );
        assert_eq!(provider.refresh_from_live().await.unwrap(), 1);
        assert_eq!(
            provider.runtime_context_limit("qwen3.8:latest"),
            Some(262_144),
            "the runtime limit never exceeds the model maximum"
        );
        // Model no longer loaded: the cached entry is evicted and the limit
        // goes back to None (model-max fallback is the safe direction).
        server.route(
            "GET",
            "/api/ps",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"llama3.2:3b","details":{"context_length":8192}}]}"#
                    .into(),
            },
        );
        assert_eq!(provider.refresh_from_live().await.unwrap(), 1);
        assert_eq!(
            provider.runtime_context_limit("qwen3.8:latest"),
            None,
            "an unloaded model has no live allocation"
        );
    }

    #[tokio::test]
    async fn runtime_context_limit_none_when_ps_never_succeeded() {
        // /api/ps hostile from the start: refresh_from_live still succeeds
        // (the ps pass is best-effort) but runtime_context_limit is None —
        // the budget uses the model maximum exactly as before the fix.
        let server = MockServer::new();
        server.route(
            "GET",
            "/api/tags",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"qwen3.8:latest"}]}"#.into(),
            },
        );
        server.route(
            "POST",
            "/api/show",
            MockAction::Respond {
                status: 200,
                body: r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":262144}}"#.into(),
            },
        );
        server.route(
            "GET",
            "/api/ps",
            MockAction::Respond {
                status: 200,
                body: "{not json".into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        assert_eq!(
            provider.refresh_from_live().await.unwrap(),
            1,
            "a hostile /api/ps must not fail warm-up"
        );
        assert_eq!(
            provider.runtime_context_limit("qwen3.8:latest"),
            None,
            "no live ps data ever: no runtime override"
        );
    }

    #[tokio::test]
    async fn runtime_context_limit_survives_a_later_hostile_ps_with_stale_value() {
        // Adversarial refresh sequence: a GOOD /api/ps observation is never
        // cleared by a later hostile one (stale-but-conservative beats
        // losing the last known allocation).
        let server = MockServer::new();
        server.route(
            "GET",
            "/api/tags",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"qwen3.8:latest"}]}"#.into(),
            },
        );
        server.route(
            "POST",
            "/api/show",
            MockAction::Respond {
                status: 200,
                body: r#"{"model_info":{"general.architecture":"qwen3","qwen3.context_length":262144}}"#.into(),
            },
        );
        let ps = |body: &str| {
            server.route(
                "GET",
                "/api/ps",
                MockAction::Respond {
                    status: 200,
                    body: body.into(),
                },
            );
        };
        ps(r#"{"models":[{"name":"qwen3.8:latest","details":{"context_length":65536}}]}"#);
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        assert_eq!(provider.refresh_from_live().await.unwrap(), 1);
        assert_eq!(
            provider.runtime_context_limit("qwen3.8:latest"),
            Some(65_536)
        );
        // A dead /api/ps (500) afterwards: the cache keeps the stale limit.
        server.route(
            "GET",
            "/api/ps",
            MockAction::Respond {
                status: 500,
                body: "boom".into(),
            },
        );
        assert_eq!(provider.refresh_from_live().await.unwrap(), 1);
        assert_eq!(
            provider.runtime_context_limit("qwen3.8:latest"),
            Some(65_536),
            "a failed refresh must never clear the last known allocation"
        );
    }

    #[test]
    fn ps_allocated_context_never_panics_on_hostile_bodies() {
        assert_eq!(
            ps_allocated_context(&serde_json::json!({"nope": 1}), "m")
                .unwrap_err()
                .kind,
            ErrorKind::Malformed
        );
        assert!(ps_allocated_context(&serde_json::json!({"models": {}}), "m").is_err());
        assert!(ps_allocated_context(&serde_json::json!({"models": [42]}), "m").is_err());
        assert!(
            ps_allocated_context(
                &serde_json::json!({"models": [{"details": {"context_length": 1}}]}),
                "m"
            )
            .is_err(),
            "an entry without a name is corrupt"
        );
        assert_eq!(
            ps_allocated_context(&serde_json::json!({"models": []}), "m").unwrap(),
            None
        );
        // Tag-less matching: entry "qwen3.8:latest" answers model "qwen3.8".
        let body = serde_json::json!({"models": [{"name": "qwen3.8:latest", "details": {"context_length": 8192}}]});
        assert_eq!(ps_allocated_context(&body, "qwen3.8").unwrap(), Some(8192));
        assert_eq!(
            ps_allocated_context(&body, "qwen3.8:latest").unwrap(),
            Some(8192)
        );
        assert_eq!(ps_allocated_context(&body, "qwen3.8-abl").unwrap(), None);
        // Non-numeric allocations are absent, never a panic.
        let str_ctx = serde_json::json!({"models": [{"name": "qwen3.8:latest", "details": {"context_length": "8192"}}]});
        assert_eq!(ps_allocated_context(&str_ctx, "qwen3.8").unwrap(), None);
        // Entry-level context_length also counts.
        let top =
            serde_json::json!({"models": [{"name": "qwen3.8:latest", "context_length": 4096}]});
        assert_eq!(ps_allocated_context(&top, "qwen3.8").unwrap(), Some(4096));
        // size_vram is BYTES, never mistaken for a token budget.
        let vram = serde_json::json!({"models": [{"name": "qwen3.8:latest", "size_vram": 999999}]});
        assert_eq!(ps_allocated_context(&vram, "qwen3.8").unwrap(), None);
    }

    #[test]
    fn context_lookup_handles_nested_and_dotted_arch_shapes() {
        let nested_section = serde_json::json!({
            "general": {"architecture": "qwen3"},
            "qwen3": {"context_length": 262144},
        });
        assert_eq!(
            context_length_from_model_info(&nested_section),
            Some(262_144)
        );
        let bare = serde_json::json!({"context_length": 32768});
        assert_eq!(context_length_from_model_info(&bare), Some(32_768));
        let hostile_negative = serde_json::json!({"llama.context_length": -1});
        assert_eq!(context_length_from_model_info(&hostile_negative), None);
        let non_numeric = serde_json::json!({"llama.context_length": "131072"});
        assert_eq!(context_length_from_model_info(&non_numeric), None);
        let no_info = serde_json::json!({});
        assert_eq!(context_length_from_model_info(&no_info), None);
    }

    #[test]
    fn native_image_payload_strips_data_uri_prefix() {
        assert_eq!(native_image_payload("data:image/png;base64,QUJD"), "QUJD");
        assert_eq!(native_image_payload("data:,YWJj"), "YWJj");
        assert_eq!(native_image_payload("QUJD"), "QUJD");
        assert_eq!(
            native_image_payload("data:no-comma"),
            "data:no-comma",
            "malformed data URIs pass through untouched"
        );
    }

    #[test]
    fn lowering_splits_mixed_part_roles_preserving_order() {
        // One generic message with parts of every kind: text stays on the
        // declared role, tool results become role-"tool" messages at their
        // position, tool calls ride an assistant message, and nothing is
        // reordered.
        let m = RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("a"),
                ContentPart::tool_result("out", false, "c1"),
                ContentPart::tool_call("c1", "read_file", serde_json::json!({"path": "x"})),
                ContentPart::reasoning("think"),
                ContentPart::text("b"),
            ],
        };
        let mut names = HashMap::new();
        let native = lower_native_message(&m, &mut names);
        assert_eq!(
            native,
            vec![
                serde_json::json!({"role": "user", "content": "a"}),
                // The result precedes its call in the parts: no tool_name.
                serde_json::json!({"role": "tool", "content": "out"}),
                // Tool call + reasoning coalesce onto ONE assistant message
                // (assistant messages may carry tool_calls and thinking
                // together — the native fields stay orthogonal).
                serde_json::json!({
                    "role": "assistant",
                    "content": "",
                    "thinking": "think",
                    "tool_calls": [{
                        "function": { "name": "read_file", "arguments": { "path": "x" } }
                    }]
                }),
                serde_json::json!({"role": "user", "content": "b"}),
            ]
        );
    }

    #[test]
    fn tool_result_names_the_call_it_answers_when_visible() {
        // The native form pairs the result with its call only when the
        // call appeared earlier in this request.
        let call_msg = RequestMessage {
            role: Role::Assistant,
            content: vec![ContentPart::tool_call(
                "call_1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )],
        };
        let result_msg = RequestMessage {
            role: Role::User,
            content: vec![ContentPart::tool_result("contents", false, "call_1")],
        };
        let mut names = HashMap::new();
        let mut native = Vec::new();
        native.extend(lower_native_message(&call_msg, &mut names));
        native.extend(lower_native_message(&result_msg, &mut names));
        assert_eq!(
            native,
            vec![
                serde_json::json!({
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "function": { "name": "read_file", "arguments": { "path": "a.rs" } }
                    }]
                }),
                serde_json::json!({
                    "role": "tool",
                    "content": "contents",
                    "tool_name": "read_file"
                }),
            ]
        );
    }

    // ------------------------------------------------------- egress (P0-36)

    fn allow_only(port: u16) -> Arc<dyn HttpTransport> {
        Arc::new(PolicyCheckedHttpTransport::with_policy(Some(
            DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{port}")]).unwrap(),
        )))
    }

    async fn first_error(mut stream: ProviderStream) -> ProviderError {
        stream
            .next()
            .await
            .expect("an item")
            .expect_err("the first item must be the deny error")
    }

    #[tokio::test]
    async fn egress_allowlist_gates_chat_and_discovery_before_connect() {
        // Streaming path (/api/chat, NDJSON): allowed streams, denied
        // fails BEFORE any network byte.
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/chat",
            MockAction::Respond {
                status: 200,
                body: r#"{"message":{"role":"assistant","content":"allowed"},"done":true}"#.into(),
            },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();
        let provider = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some(base.clone())),
            allow_only(port),
        );
        let chunks = stream_chunks(&*provider, req("qwen3.8")).await;
        assert_eq!(
            chunks.first(),
            Some(&ProviderChunk::Text {
                text: "allowed".into()
            })
        );
        assert!(matches!(chunks.last(), Some(ProviderChunk::Done)));
        assert_eq!(server.request_count(), 1);

        let denied = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some(base.clone())),
            allow_only(port.wrapping_add(1)),
        );
        let err = first_error(denied.stream(req("qwen3.8"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert!(!err.retryable, "denied destinations are never retried");
        assert_eq!(server.request_count(), 1, "deny happened before connect");

        // https-to-http mismatch against an https-only rule: pre-connect deny.
        let mismatch = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some(base.clone())),
            Arc::new(PolicyCheckedHttpTransport::with_policy(Some(
                DestinationPolicy::parse_lines([&format!("https://127.0.0.1:{port}")]).unwrap(),
            ))),
        );
        let err = first_error(mismatch.stream(req("qwen3.8"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 1, "scheme mismatch: no connect");

        // Non-streaming discovery GET (/api/tags) rides the SAME transport:
        // allowed here, denied pre-connect on the wrong-port instance.
        server.route(
            "GET",
            "/api/tags",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"qwen3.8:latest"}]}"#.into(),
            },
        );
        let provider = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some(base.clone())),
            allow_only(port),
        );
        let models = provider.discover_models().await.unwrap();
        assert_eq!(models, vec!["qwen3.8:latest"]);
        assert_eq!(server.request_count(), 2);

        let denied = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some(base)),
            allow_only(port.wrapping_add(1)),
        );
        let err = denied.discover_models().await.unwrap_err();
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 2, "discovery deny pre-connect");
    }

    #[tokio::test]
    async fn mock_transport_canned_ndjson_drives_the_parser_without_http() {
        let body = r#"{"message":{"role":"assistant","thinking":"hmm","content":"can"},"done":false}
{"message":{"role":"assistant","content":"ned"},"done":true}
"#;
        let mock = Arc::new(MockHttpTransport::new(200, body));
        let as_transport: Arc<dyn HttpTransport> = mock.clone();
        let provider = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            as_transport,
        );
        let chunks = stream_chunks(&*provider, req("qwen3.8")).await;
        let mut kinds = Vec::new();
        let mut text = String::new();
        for chunk in &chunks {
            match chunk {
                ProviderChunk::Reasoning { text } => {
                    assert_eq!(text, "hmm");
                    kinds.push("reasoning");
                }
                ProviderChunk::Text { text: t } => {
                    text.push_str(t);
                    kinds.push("text");
                }
                ProviderChunk::Done => kinds.push("done"),
                other => panic!("unexpected chunk {other:?}"),
            }
        }
        assert_eq!(kinds, ["reasoning", "text", "text", "done"]);
        assert_eq!(text, "canned");
        assert_eq!(mock.request_count(), 1);
        assert_eq!(
            mock.requests(),
            vec![(
                "POST".to_string(),
                "http://mock.invalid/api/chat".to_string()
            )]
        );
    }

    // ------------------------------------------------- embeddings

    fn embed_req(model: &str, inputs: &[&str]) -> EmbeddingRequest {
        EmbeddingRequest::new(model, inputs.iter().map(|s| (*s).to_string()).collect())
            .expect("test embedding request is within bounds")
    }

    fn embed_meta(deadline_ms: u64) -> RequestMeta {
        RequestMeta {
            operation_id: OpId::new(7),
            session_id: SessionId::new(3),
            provider: "ollama".into(),
            attempt: 0,
            deadline_ms,
            cancellation: CancellationToken::new(),
        }
    }

    /// Byte-exact `/api/embed` lowering against the mock: the model, the
    /// ordered batch input and the configured `keep_alive` land on the wire —
    /// and nothing else (no internal metadata); the response lowers to one
    /// vector per input, in input order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embed_request_and_response_lower_byte_exact() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/api/embed",
            MockAction::AssertThenRespond {
                status: 200,
                body: r#"{"model":"all-minilm","embeddings":[[0.5,-0.25],[1.0,0.0]]}"#.into(),
                assert: Arc::new(|body: &serde_json::Value| {
                    assert_eq!(body["model"], "all-minilm");
                    assert_eq!(body["input"], serde_json::json!(["alpha", "beta"]));
                    assert!(!body.as_object().unwrap().contains_key("options"));
                    for leaked in [
                        "operation_id",
                        "session_id",
                        "attempt",
                        "deadline_ms",
                        "cancellation",
                    ] {
                        assert!(
                            !body.as_object().unwrap().contains_key(leaked),
                            "{leaked} leaked!"
                        );
                    }
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        let out = provider
            .embed(embed_req("all-minilm", &["alpha", "beta"]))
            .unwrap();
        assert_eq!(out.vectors, vec![vec![0.5, -0.25], vec![1.0, 0.0]]);
        let (method, path, raw) = server.last_request().unwrap();
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/api/embed"));
        // Wire-exact contract, feature-unification-proof: the JSON VALUE
        // equals {model, input, keep_alive} exactly and no other key is
        // present. Raw byte ordering is NOT asserted because serde_json's
        // Map ordering depends on the `preserve_order` feature being
        // unified in by any workspace crate.
        let sent: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "model": "all-minilm",
                "input": ["alpha", "beta"],
                "keep_alive": "30m",
            })
        );
        let keys: Vec<&str> = sent
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec!["input", "keep_alive", "model"]);
    }

    /// Hostile `/api/embed` bodies are typed `Malformed` refusals — wrong
    /// dimension, ragged batches, NaN/infinite components, oversized
    /// dimensions, wrong vector counts and mistyped shapes never panic and
    /// never fabricate a vector.
    #[tokio::test]
    async fn hostile_embed_responses_are_typed_never_panicking() {
        let oversized = format!(
            r#"{{"embeddings":[[{}]]}}"#,
            vec!["0.0"; faktor_provider::MAX_EMBEDDING_DIMENSIONS + 1].join(",")
        );
        let cases: Vec<(&str, String)> = vec![
            ("missing array", r#"{"model":"m"}"#.into()),
            ("mistyped array", r#"{"embeddings":"nope"}"#.into()),
            ("mistyped entry", r#"{"embeddings":[1.0]}"#.into()),
            ("ragged batch", r#"{"embeddings":[[1.0,2.0],[3.0]]}"#.into()),
            (
                "non-finite",
                r#"{"embeddings":[[1e400,2.0],[0.0,1.0]]}"#.into(),
            ),
            ("wrong count", r#"{"embeddings":[[1.0,2.0]]}"#.into()),
            ("oversized dimension", oversized),
        ];
        for (name, body) in cases {
            let mock = Arc::new(MockHttpTransport::new(200, body));
            let provider = OllamaProvider::new_with_transport(
                OllamaConfig::new(Some("http://mock.invalid".into())),
                mock.clone(),
            );
            let err = provider
                .embed(embed_req("m", &["alpha", "beta"]))
                .unwrap_err();
            assert_eq!(err.kind, ProviderErrorKind::Malformed, "{name}: {err:?}");
            assert!(!err.retryable, "{name}: a hostile body is terminal");
        }
        // An over-cap body is refused before any parse (bounded read).
        let mock = Arc::new(MockHttpTransport::new(
            200,
            "x".repeat(EMBED_RESPONSE_MAX_BYTES + 1),
        ));
        let provider = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            mock,
        );
        let err = provider.embed(embed_req("m", &["a"])).unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(err.message.contains("exceeds"), "{err}");
    }

    /// Status and transport classes lower to the exact typed error the retry
    /// policy consumes: 5xx/429/network are retryable, 401/4xx and
    /// policy/build refusals are terminal.
    #[tokio::test]
    async fn embed_errors_are_retry_classified() {
        let cases: [(u16, ProviderErrorKind, bool); 4] = [
            (401, ProviderErrorKind::Auth, false),
            (429, ProviderErrorKind::RateLimited, true),
            (500, ProviderErrorKind::Server, true),
            (400, ProviderErrorKind::BadRequest, false),
        ];
        for (status, kind, retryable) in cases {
            let mock = Arc::new(MockHttpTransport::new(status, "denied"));
            let provider = OllamaProvider::new_with_transport(
                OllamaConfig::new(Some("http://mock.invalid".into())),
                mock,
            );
            let err = provider.embed(embed_req("m", &["a"])).unwrap_err();
            assert_eq!(err.kind, kind, "status {status}");
            assert_eq!(err.retryable, retryable, "status {status}");
            assert_eq!(err.code.as_deref(), Some(status.to_string().as_str()));
        }
        // Transport failure = retryable Network; a refused destination
        // (policy/build layer) is a terminal BadRequest.
        let transport_err = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            Arc::new(MockHttpTransport::denying(
                faktor_provider::egress::EgressError::Transport("connection reset".into()),
            )),
        )
        .embed(embed_req("m", &["a"]))
        .unwrap_err();
        assert_eq!(transport_err.kind, ProviderErrorKind::Network);
        assert!(transport_err.retryable);
        let policy_err = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            Arc::new(MockHttpTransport::denying(
                faktor_provider::egress::EgressError::UnparseableUrl("nope".into()),
            )),
        )
        .embed(embed_req("m", &["a"]))
        .unwrap_err();
        assert_eq!(policy_err.kind, ProviderErrorKind::BadRequest);
        assert!(!policy_err.retryable, "a policy refusal must never retry");
    }

    /// A transport that never answers: the operation deadline from
    /// `RequestMeta` must fire, typed `Timeout`, and the error must name the
    /// operation/session lineage it came from.
    struct SilentTransport;

    impl HttpTransport for SilentTransport {
        fn execute(
            &self,
            _req: reqwest::Request,
        ) -> futures::future::BoxFuture<
            '_,
            Result<reqwest::Response, faktor_provider::egress::EgressError>,
        > {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                Err(faktor_provider::egress::EgressError::Transport(
                    "never answered".into(),
                ))
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn embed_honors_the_operation_deadline_with_lineage() {
        let provider = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            Arc::new(SilentTransport),
        );
        let started = std::time::Instant::now();
        let err = provider
            .embed(embed_req("m", &["a"]).with_meta(embed_meta(40)))
            .unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert_eq!(err.code.as_deref(), Some("deadline"));
        assert!(err.message.contains("operation 7"), "{err}");
        assert!(err.message.contains("session 3"), "{err}");
        // A zero deadline keeps the adapter's own first-byte fallback bound
        // (never unbounded); a fast mock answers well inside it.
        let mock = Arc::new(MockHttpTransport::new(200, r#"{"embeddings":[[1.0]]}"#));
        let provider = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            mock,
        );
        let out = provider.embed(embed_req("m", &["a"])).unwrap();
        assert_eq!(out.vectors, vec![vec![1.0]]);
    }

    /// The embedding capability flag is set from the probed `/api/show`
    /// capabilities (both real spellings) or an operator override, and only
    /// then does `supports_embeddings` admit the model.
    #[tokio::test]
    async fn embedding_capability_flag_drives_supports_embeddings() {
        let server = MockServer::new();
        server.route(
            "GET",
            "/api/tags",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[{"name":"embed-a"},{"name":"embed-b"},{"name":"chat-only"}]}"#
                    .into(),
            },
        );
        server.route(
            "POST",
            "/api/show",
            MockAction::Sequence {
                // Discovery sorts names: chat-only, embed-a, embed-b.
                actions: vec![
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"capabilities":["completion","tools"]}"#.into(),
                    },
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"capabilities":["embedding"]}"#.into(),
                    },
                    MockAction::Respond {
                        status: 200,
                        body: r#"{"capabilities":["embeddings"]}"#.into(),
                    },
                ],
            },
        );
        server.route(
            "GET",
            "/api/ps",
            MockAction::Respond {
                status: 200,
                body: r#"{"models":[]}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OllamaProvider::new(OllamaConfig::new(Some(base)));
        assert_eq!(provider.refresh_from_live().await.unwrap(), 3);
        assert!(provider.capabilities("embed-a").embeddings);
        assert!(provider.supports_embeddings("embed-a"));
        assert!(provider.capabilities("embed-b").embeddings);
        assert!(provider.supports_embeddings("embed-b"));
        assert!(!provider.capabilities("chat-only").embeddings);
        assert!(!provider.supports_embeddings("chat-only"));
        // Unprobed models keep the conservative small-local default, which
        // advertises embeddings: `/api/embed` serves every Ollama model, so
        // the strict `[embeddings]` selection resolves before a probe.
        assert!(provider.supports_embeddings("never-probed"));
        // An operator override is authoritative.
        let mut cfg = OllamaConfig::new(Some("http://127.0.0.1:1".into()));
        let mut caps = ModelCapabilities::small_local();
        caps.embeddings = true;
        cfg.model_overrides.insert("pinned".into(), caps);
        let provider = OllamaProvider::new(cfg);
        assert!(provider.supports_embeddings("pinned"));
    }

    /// Model and batch are required/re-bounded: a hostile
    /// directly-constructed request (public fields) is refused typedly
    /// BEFORE any wire byte, even though the typed constructor is bypassed.
    #[tokio::test]
    async fn embed_requires_a_model_and_a_bounded_batch_before_any_wire_byte() {
        let mock = Arc::new(MockHttpTransport::new(200, r#"{"embeddings":[[1.0]]}"#));
        let provider = OllamaProvider::new_with_transport(
            OllamaConfig::new(Some("http://mock.invalid".into())),
            mock.clone(),
        );
        let hostile = EmbeddingRequest {
            model: "  ".into(),
            inputs: vec!["x".into()],
            meta: None,
        };
        let err = provider.embed(hostile).unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(!err.retryable);
        let oversized = EmbeddingRequest {
            model: "m".into(),
            inputs: vec!["x".into(); faktor_provider::MAX_EMBEDDING_INPUTS + 1],
            meta: None,
        };
        let err = provider.embed(oversized).unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("over the cap"), "{err}");
        let empty = EmbeddingRequest {
            model: "m".into(),
            inputs: Vec::new(),
            meta: None,
        };
        assert_eq!(
            provider.embed(empty).unwrap_err().kind,
            ProviderErrorKind::BadRequest
        );
        assert_eq!(mock.request_count(), 0, "no wire byte for a hostile batch");
    }

    // ------------------------------------------------- canonical usage

    /// Shared canonical-usage conformance for the native /api/chat wire
    /// (audit Phase-1 item C): mock NDJSON bodies shaped exactly like real
    /// final frames (top-level `prompt_eval_count` / `eval_count` counters,
    /// optional thinking/content on the same frame) drive the REAL
    /// provider.
    mod canonical_usage_conformance {
        use super::*;
        use faktor_provider::canonical_usage_conformance;
        use faktor_provider::CanonicalUsage;

        fn ndjson(v: serde_json::Value) -> String {
            format!("{v}\n")
        }

        fn exp(uncached: u64, output: u64) -> CanonicalUsage {
            CanonicalUsage {
                uncached_input_tokens: uncached,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: output,
                reasoning_tokens: 0,
                reported_cost: None,
                request_id: None,
            }
        }

        canonical_usage_conformance! {
            driver: ollama_native_canonical_usage_conformance,
            family: faktor_provider::usage_conformance::WireFamily::NoCacheDetail,
            label: "ollama native /api/chat",
            request: || req("qwen3.8"),
            provider: |base: String| OllamaProvider::build(OllamaConfig::new(Some(base))),
            method: "POST",
            path: "/api/chat",
            cases: vec![
                // The API exposes NO cache split: the conservative correct
                // category is uncached = the reported evaluated count.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "counts_map_uncached_total",
                    ndjson(serde_json::json!({
                        "done": true, "prompt_eval_count": 1000, "eval_count": 50,
                    })),
                    exp(1000, 50),
                ),
                // Thinking tokens are part of the generation
                // (eval_count already contains them) and the API reports no
                // separate thinking count — reasoning is never double
                // billed, and the frame follows the same-frame text.
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "thinking_included_in_output_never_double_billed",
                    ndjson(serde_json::json!({
                        "message": {"role": "assistant", "thinking": "hmm", "content": "hi"},
                        "done": true, "prompt_eval_count": 1000, "eval_count": 50,
                    })),
                    exp(1000, 50),
                ),
                // Hostile wrong-typed/negative counters are ignored (0) and
                // yield no usage frame — never a panic, never an error.
                faktor_provider::usage_conformance::WireUsageCase::no_usage(
                    "hostile_junk_counts_never_panic",
                    ndjson(serde_json::json!({
                        "done": true,
                        "prompt_eval_count": "many",
                        "eval_count": -3,
                        "eval_count_duration": [],
                        "unknown_final": {"deep": [1, 2]},
                    })),
                ),
                // An all-zero counter row carries nothing.
                faktor_provider::usage_conformance::WireUsageCase::no_usage(
                    "zero_counts_no_usage_frame",
                    ndjson(serde_json::json!({
                        "done": true, "prompt_eval_count": 0, "eval_count": 0,
                    })),
                ),
            ]
        }
    }
}
