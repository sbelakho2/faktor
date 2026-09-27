//! `config::provider`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The wire family of an `open_ai` provider entry.
///
/// - `chat` (default for CUSTOM endpoints): `POST /chat/completions` — the
///   classic shape every OpenAI-compatible server implements.
/// - `responses` (default for the OFFICIAL `api.openai.com` endpoint): the
///   native Responses API (`POST /responses`).
///
/// The default is chosen from the configured `base_url` when `api` is
/// absent; an explicit `api` always wins (including `api = "chat"` on the
/// official endpoint, for deployments/proxies that only speak Chat
/// Completions). Any other value is a strict parse error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApi {
    Chat,
    Responses,
}

/// The typed default of [`ProviderCfg::Ollama::allow_loopback`]: the local
/// runtime's own documented endpoint is loopback.
pub(crate) fn default_ollama_allow_loopback() -> bool {
    true
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCfg {
    Ollama {
        id: String,
        base_url: Option<String>,
        /// Explicit loopback address-class rule for this LOCAL runtime: its
        /// documented default endpoint is `http://127.0.0.1:11434`, so the
        /// typed default is `true`. Set `false` to refuse loopback for this
        /// entry. Never a global exception: every other special address
        /// class (private, link-local/metadata, CGNAT, documentation, ...)
        /// stays refused for every provider.
        #[serde(default = "default_ollama_allow_loopback")]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
        /// The additive per-instance quality declaration (quality-authority
        /// audit item 1): authorizes this endpoint's models for quality
        /// floors with explicit `UserConfigured` provenance. Never applied
        /// to any other instance.
        #[serde(default)]
        quality: Option<ProviderQualityCfg>,
    },
    OpenAi {
        id: String,
        base_url: String,
        api_key_env: Option<String>,
        /// Wire family override. Absent = the documented endpoint default:
        /// the official OpenAI endpoint prefers the modern Responses API
        /// while every custom `base_url` stays on Chat Completions (custom
        /// compatible servers rarely implement `/responses`). Explicit
        /// `chat`/`responses` always wins; any other value fails to parse.
        #[serde(default)]
        api: Option<OpenAiApi>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
        /// The additive per-instance quality declaration (quality-authority
        /// audit item 1): authorizes this endpoint's models for quality
        /// floors with explicit `UserConfigured` provenance. Never applied
        /// to any other instance.
        #[serde(default)]
        quality: Option<ProviderQualityCfg>,
    },
    Anthropic {
        id: String,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
        /// The additive per-instance quality declaration (quality-authority
        /// audit item 1): authorizes this endpoint's models for quality
        /// floors with explicit `UserConfigured` provenance. Never applied
        /// to any other instance.
        #[serde(default)]
        quality: Option<ProviderQualityCfg>,
    },
    Google {
        id: String,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
        /// The additive per-instance quality declaration (quality-authority
        /// audit item 1): authorizes this endpoint's models for quality
        /// floors with explicit `UserConfigured` provenance. Never applied
        /// to any other instance.
        #[serde(default)]
        quality: Option<ProviderQualityCfg>,
    },
    DeepSeek {
        id: String,
        profile: String,
        base_url: Option<String>,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
        /// The additive per-instance quality declaration (quality-authority
        /// audit item 1): authorizes this endpoint's models for quality
        /// floors with explicit `UserConfigured` provenance. Never applied
        /// to any other instance.
        #[serde(default)]
        quality: Option<ProviderQualityCfg>,
    },
    Gateway {
        id: String,
        base_url: String,
        api_key_env: Option<String>,
        /// Explicit loopback address-class rule (default `false`: a remote
        /// endpoint whose host resolves onto loopback is a rebinding
        /// signal). `true` is the deliberate opt-in for a local
        /// OpenAI-compatible proxy/runtime. Only loopback can be opted
        /// into; no other special class is ever allowed.
        #[serde(default)]
        allow_loopback: bool,
        #[serde(default)]
        pricing: Option<ProviderPricingCfg>,
        /// The additive per-instance quality declaration (quality-authority
        /// audit item 1): authorizes this endpoint's models for quality
        /// floors with explicit `UserConfigured` provenance. Never applied
        /// to any other instance.
        #[serde(default)]
        quality: Option<ProviderQualityCfg>,
    },
}

/// The additive per-provider `pricing` override section (audit P0-1 /
/// wave-B item A): prices are microUSD PER MILLION TOKENS — the exact unit
/// providers publish — so sub-$1/M list prices ($0.50/M = 500_000 microUSD
/// per million tokens) are representable and can never truncate to a free
/// lie. Example: `{"pricing": {"input_micro_usd_per_million_tokens":
/// 2_500_000, "output_micro_usd_per_million_tokens": 10_000_000}}` ($2.50/M
/// in, $10/M out) inside ONE `providers[]` entry, scoped to that entry's
/// `id`. This is the money surface that makes REAL production economics
/// reach the routing graph — without it, remote (OpenAI-compatible/gateway/
/// deepseek/anthropic/google) models are catalog-priced
/// [`PricingState::Unknown`] (unless the built-in table documents them) and
/// the Economy candidate set EXCLUDES the Unknown ones (no fake zero
/// prices, no 1-microUSD fallback).
///
/// Two independent knobs:
///
/// - exact prices: `input_micro_usd_per_million_tokens` + `output_micro_usd_per_million_tokens`
///   (both REQUIRED together, each >= 1 microUSD per million tokens), plus optional
///   `cache_read_*`/`cache_write_*` (0 = the endpoint publishes no cache price). They price
///   EVERY model the endpoint serves at the declared per-million microUSD values
///   (state `Known`, provenance `UserOverride`). Intended for custom OpenAI-compatible
///   endpoints whose real prices the operator knows; overrides NEVER apply to a local
///   runtime (Ollama rows are `LocalZero` and stay zero).
/// - `pricing_ceiling_micro_usd_per_million_tokens`: a CONSERVATIVE budget bound that prices
///   ONLY models the adapter itself leaves Unknown, at the ceiling on all four price lines
///   (state `ConservativeCeiling`, provenance `Composite` — a ceiling, never a measured
///   price). Known-priced and LocalZero models are untouched.
///
/// Both knobs bump the catalog row's `source_epoch` so settlement can tell
/// the price generation changed. Unknown keys inside `pricing` are parse
/// errors; hostile values (0 input, absurd magnitudes, a table on a local
/// provider) are typed validation errors.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderPricingCfg {
    /// Exact override, microUSD per MILLION tokens.
    pub input_micro_usd_per_million_tokens: Option<u64>,
    pub output_micro_usd_per_million_tokens: Option<u64>,
    pub cache_read_micro_usd_per_million_tokens: Option<u64>,
    pub cache_write_micro_usd_per_million_tokens: Option<u64>,
    /// Conservative ceiling, microUSD per million tokens, applied to
    /// Unknown-priced models only.
    pub pricing_ceiling_micro_usd_per_million_tokens: Option<u64>,
}

/// Ceiling and exact-price magnitude cap (microUSD per million tokens).
/// 1_000_000_000_000 microUSD/million tokens = $1M per million tokens —
/// beyond any production model; anything larger is a hostile/absurd config
/// value.
pub const MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS: u64 = 1_000_000_000_000;

impl ProviderPricingCfg {
    /// True when the section configures anything at all.
    pub fn is_empty(&self) -> bool {
        self == &ProviderPricingCfg::default()
    }

    /// Typed validation of the override surface. Rules:
    ///
    /// - a pricing section under a LOCAL provider (kind `ollama`) is
    ///   refused (Ollama rows are measured LocalZero; a config table must
    ///   never make a local runtime look paid, nor is it honored);
    /// - exact input/output prices are REQUIRED as a pair and each must
    ///   be >= 1 microUSD — `0` is the local-free marker, and a remote
    ///   endpoint must never silently read as free — and <= the magnitude
    ///   cap;
    /// - cache prices are optional, 0 allowed (= no cache price), capped;
    /// - the ceiling must be >= 1 microUSD and <= the cap (a zero ceiling
    ///   would price unknown models as free — the exact lie this audit
    ///   removes).
    pub fn validate(&self, kind: &str) -> Result<(), String> {
        if self.is_empty() {
            return Ok(());
        }
        if kind == "ollama" {
            return Err(
                "pricing overrides on a local (ollama) provider are refused: \
                 ollama rows are measured local-zero cost and a pricing table would only \
                 fabricate a paid profile (id-scoped override tables apply to custom \
                 OpenAI-compatible REMOTE endpoints only)"
                    .to_string(),
            );
        }
        let exact_present = self.input_micro_usd_per_million_tokens.is_some()
            || self.output_micro_usd_per_million_tokens.is_some()
            || self.cache_read_micro_usd_per_million_tokens.is_some()
            || self.cache_write_micro_usd_per_million_tokens.is_some();
        if exact_present {
            let (Some(input), Some(output)) = (
                self.input_micro_usd_per_million_tokens,
                self.output_micro_usd_per_million_tokens,
            ) else {
                return Err(
                    "pricing override table must set BOTH input_micro_usd_per_million_tokens \
                     and output_micro_usd_per_million_tokens (a partial table would silently \
                     price the missing side at 0 microUSD)"
                        .to_string(),
                );
            };
            if input == 0 || output == 0 {
                return Err(
                    "pricing override input/output prices of 0 are refused: 0 microUSD is the \
                     LOCAL-free marker and a remote endpoint must never silently read as free"
                        .to_string(),
                );
            }
            for (name, v) in [
                ("input_micro_usd_per_million_tokens", input),
                ("output_micro_usd_per_million_tokens", output),
                (
                    "cache_read_micro_usd_per_million_tokens",
                    self.cache_read_micro_usd_per_million_tokens.unwrap_or(0),
                ),
                (
                    "cache_write_micro_usd_per_million_tokens",
                    self.cache_write_micro_usd_per_million_tokens.unwrap_or(0),
                ),
            ] {
                if v > MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS {
                    return Err(format!(
                        "{name} = {v} exceeds the magnitude cap of \
                         {MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS} microUSD per million tokens"
                    ));
                }
            }
        }
        if let Some(c) = self.pricing_ceiling_micro_usd_per_million_tokens {
            if c == 0 {
                return Err(
                    "pricing_ceiling_micro_usd_per_million_tokens of 0 is refused: a zero \
                     ceiling would price unknown models as free"
                        .to_string(),
                );
            }
            if c > MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS {
                return Err(format!(
                    "pricing_ceiling_micro_usd_per_million_tokens = {c} exceeds the magnitude \
                     cap of {MAX_PRICING_MICRO_USD_PER_MILLION_TOKENS} microUSD per million \
                     tokens"
                ));
            }
        }
        Ok(())
    }

    /// Map the parsed config onto the provider crate's override policy
    /// (per-million-token quotes; exact = a real [`PriceQuote`], ceiling =
    /// the conservative per-million bound).
    pub(crate) fn to_overrides(&self) -> PricingOverrides {
        let exact = match (
            self.input_micro_usd_per_million_tokens,
            self.output_micro_usd_per_million_tokens,
        ) {
            (Some(input), Some(output)) => Some(PriceQuote {
                input: MicroUsdPerMillionTokens(input),
                output: MicroUsdPerMillionTokens(output),
                cache_read: MicroUsdPerMillionTokens(
                    self.cache_read_micro_usd_per_million_tokens.unwrap_or(0),
                ),
                cache_write: MicroUsdPerMillionTokens(
                    self.cache_write_micro_usd_per_million_tokens.unwrap_or(0),
                ),
            }),
            _ => None,
        };
        PricingOverrides {
            exact,
            ceiling_micro_usd_per_million_tokens: self
                .pricing_ceiling_micro_usd_per_million_tokens
                .map(MicroUsdPerMillionTokens),
        }
    }
}

/// The additive per-provider `quality` declaration section (quality-
/// authority audit item 1): the user's explicit reliability statement for
/// ONE configured instance — coding/context/tool/reasoning reliability,
/// each range-checked `0..=100`. Example:
/// `{"quality": {"coding_reliability": 72, "context_reliability": 80}}`.
///
/// This is what AUTHORIZES a row whose quality would otherwise be
/// `ConservativeUnknown` (a local/unknown endpoint) to clear quality
/// floors — the dead-end removal: without a declaration, an unknown row's
/// neutral placeholder never clears a floor and the operator has no
/// declared way to authorize it. Durable verified routing outcomes
/// supersede the declaration; the declaration never applies to another
/// instance, and it never manufactures a measurement (provenance stays
/// `UserConfigured`).
///
/// Unknown keys are parse errors; an empty table or any out-of-range value
/// is a typed validation error.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderQualityCfg {
    pub coding_reliability: Option<u8>,
    pub context_reliability: Option<u8>,
    pub tool_reliability: Option<u8>,
    pub reasoning_reliability: Option<u8>,
}

impl ProviderQualityCfg {
    /// True when no dimension is declared (the section's empty sentinel).
    pub fn is_empty(&self) -> bool {
        self == &ProviderQualityCfg::default()
    }

    /// Typed validation: at least one dimension must be declared, and every
    /// declared value must be in `0..=100` (the audit's range). `0` is a
    /// REAL declaration (this endpoint is useless on that dimension) — it
    /// is not an error.
    pub fn validate(&self) -> Result<(), String> {
        if self.is_empty() {
            return Err(
                "quality declaration is empty: declare at least one of coding_reliability, \
                 context_reliability, tool_reliability, reasoning_reliability in 0..=100"
                    .to_string(),
            );
        }
        for (name, value) in [
            ("coding_reliability", self.coding_reliability),
            ("context_reliability", self.context_reliability),
            ("tool_reliability", self.tool_reliability),
            ("reasoning_reliability", self.reasoning_reliability),
        ] {
            if let Some(v) = value {
                if v > 100 {
                    return Err(format!(
                        "quality declaration {name} = {v} is out of range 0..=100"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Map the parsed config onto the provider crate's declaration policy.
    pub(crate) fn to_overrides(&self) -> faktor_provider::catalog::QualityOverrides {
        faktor_provider::catalog::QualityOverrides {
            coding_reliability: self.coding_reliability,
            context_reliability: self.context_reliability,
            tool_reliability: self.tool_reliability,
            reasoning_reliability: self.reasoning_reliability,
        }
    }
}

/// The canonical official OpenAI API base URL (the ONLY OpenAI `base_url`
/// that resolves to [`BillingOrigin::OfficialOpenAi`]).
pub const OPENAI_OFFICIAL_BASE_URL: &str = "https://api.openai.com/v1";

/// The canonical official DeepSeek API base URL (the ONLY DeepSeek
/// `base_url` — besides the absent default — that resolves to
/// [`BillingOrigin::OfficialDeepSeek`]).
pub const DEEPSEEK_OFFICIAL_BASE_URL: &str = "https://api.deepseek.com";

/// Strict endpoint identity: exact string apart from surrounding
/// whitespace and a trailing slash.
pub(crate) fn same_endpoint(configured: &str, canonical: &str) -> bool {
    configured.trim().trim_end_matches('/') == canonical.trim_end_matches('/')
}

impl ProviderCfg {
    pub fn id(&self) -> &str {
        match self {
            ProviderCfg::Ollama { id, .. }
            | ProviderCfg::OpenAi { id, .. }
            | ProviderCfg::Anthropic { id, .. }
            | ProviderCfg::Google { id, .. }
            | ProviderCfg::DeepSeek { id, .. }
            | ProviderCfg::Gateway { id, .. } => id,
        }
    }

    /// The explicit loopback address-class rule this entry wires into its
    /// egress transport: `true` ONLY when this entry's typed config expects
    /// loopback (the local Ollama runtime by default, or a local
    /// OpenAI-compatible proxy that opted in). Everything else stays
    /// external-only; no other special address class is configurable.
    pub fn allows_loopback(&self) -> bool {
        match self {
            ProviderCfg::Ollama { allow_loopback, .. }
            | ProviderCfg::OpenAi { allow_loopback, .. }
            | ProviderCfg::Anthropic { allow_loopback, .. }
            | ProviderCfg::Google { allow_loopback, .. }
            | ProviderCfg::DeepSeek { allow_loopback, .. }
            | ProviderCfg::Gateway { allow_loopback, .. } => *allow_loopback,
        }
    }

    /// The configured endpoint URL of this entry, when it has one (the
    /// config-load destination-class validation inspects literal IPs here).
    pub(crate) fn configured_base_url(&self) -> Option<&str> {
        match self {
            ProviderCfg::Ollama { base_url, .. } => base_url.as_deref(),
            ProviderCfg::OpenAi { base_url, .. } => Some(base_url.as_str()),
            ProviderCfg::Anthropic { .. } | ProviderCfg::Google { .. } => None,
            ProviderCfg::DeepSeek { base_url, .. } => base_url.as_deref(),
            ProviderCfg::Gateway { base_url, .. } => Some(base_url.as_str()),
        }
    }

    /// The transport family of this entry (used to gate the pricing
    /// override surface: local runtimes refuse override tables).
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            ProviderCfg::Ollama { .. } => "ollama",
            ProviderCfg::OpenAi { .. } => "open_ai",
            ProviderCfg::Anthropic { .. } => "anthropic",
            ProviderCfg::Google { .. } => "google",
            ProviderCfg::DeepSeek { .. } => "deepseek",
            ProviderCfg::Gateway { .. } => "gateway",
        }
    }

    /// The OpenAI wire family this entry selects (`None` for non-`open_ai`
    /// entries). An explicit `api` always wins; absent, the OFFICIAL OpenAI
    /// endpoint defaults to the native Responses family (the modern default
    /// for api.openai.com) while any custom `base_url` stays on Chat
    /// Completions (compatible servers rarely implement `/responses`).
    pub fn openai_family(&self) -> Option<faktor_openai::OpenAiFamily> {
        match self {
            ProviderCfg::OpenAi { base_url, api, .. } => Some(match api {
                Some(OpenAiApi::Chat) => faktor_openai::OpenAiFamily::Chat,
                Some(OpenAiApi::Responses) => faktor_openai::OpenAiFamily::Responses,
                None => {
                    if same_endpoint(base_url, OPENAI_OFFICIAL_BASE_URL)
                        || same_endpoint(base_url, "https://api.openai.com")
                    {
                        faktor_openai::OpenAiFamily::Responses
                    } else {
                        faktor_openai::OpenAiFamily::Chat
                    }
                }
            }),
            _ => None,
        }
    }

    /// The configured endpoint's STRICT billing origin (billing-origin
    /// audit): official canonical endpoints resolve to their official
    /// origin, any other `base_url` is a [`BillingOrigin::CustomEndpoint`]
    /// (whatever transport family it speaks), `kind = gateway` and the
    /// deepseek gateway/openrouter profiles are [`BillingOrigin::Gateway`],
    /// and Ollama is [`BillingOrigin::Local`]. The origin is decided by the
    /// ENDPOINT CONFIG ONLY — the instance id (and the adapter's transport
    /// family) never changes it.
    pub fn billing_origin(&self) -> BillingOrigin {
        match self {
            ProviderCfg::Ollama { .. } => BillingOrigin::Local,
            ProviderCfg::OpenAi { base_url, .. } => {
                if same_endpoint(base_url, OPENAI_OFFICIAL_BASE_URL)
                    || same_endpoint(base_url, "https://api.openai.com")
                {
                    BillingOrigin::OfficialOpenAi
                } else {
                    BillingOrigin::CustomEndpoint
                }
            }
            ProviderCfg::Anthropic { .. } => BillingOrigin::OfficialAnthropic,
            ProviderCfg::Google { .. } => BillingOrigin::OfficialGoogle,
            ProviderCfg::DeepSeek {
                profile, base_url, ..
            } => match profile.as_str() {
                "direct" => match base_url.as_deref() {
                    None => BillingOrigin::OfficialDeepSeek,
                    Some(b)
                        if same_endpoint(b, DEEPSEEK_OFFICIAL_BASE_URL)
                            || same_endpoint(b, "https://api.deepseek.com/v1") =>
                    {
                        BillingOrigin::OfficialDeepSeek
                    }
                    Some(_) => BillingOrigin::CustomEndpoint,
                },
                // Both gateway-shaped profiles bill through an aggregator:
                // never the official DeepSeek list price.
                "gateway" | "openrouter" => BillingOrigin::Gateway,
                _ => BillingOrigin::CustomEndpoint,
            },
            ProviderCfg::Gateway { .. } => BillingOrigin::Gateway,
        }
    }

    /// The pricing override section of this entry, when configured.
    pub fn pricing(&self) -> Option<&ProviderPricingCfg> {
        match self {
            ProviderCfg::Ollama { pricing, .. }
            | ProviderCfg::OpenAi { pricing, .. }
            | ProviderCfg::Anthropic { pricing, .. }
            | ProviderCfg::Google { pricing, .. }
            | ProviderCfg::DeepSeek { pricing, .. }
            | ProviderCfg::Gateway { pricing, .. } => pricing.as_ref(),
        }
    }

    /// The quality declaration section of this entry, when configured.
    pub fn quality(&self) -> Option<&ProviderQualityCfg> {
        match self {
            ProviderCfg::Ollama { quality, .. }
            | ProviderCfg::OpenAi { quality, .. }
            | ProviderCfg::Anthropic { quality, .. }
            | ProviderCfg::Google { quality, .. }
            | ProviderCfg::DeepSeek { quality, .. }
            | ProviderCfg::Gateway { quality, .. } => quality.as_ref(),
        }
    }

    /// Validate this entry's quality declaration surface (typed errors:
    /// empty tables, out-of-range values). Called by strict config
    /// validation and by the adapter build.
    pub fn validate_quality(&self) -> Result<(), String> {
        match self.quality() {
            Some(q) => q.validate(),
            None => Ok(()),
        }
    }

    /// Validate this entry's pricing override surface (typed errors for
    /// hostile values: 0 input, absurd magnitudes, tables on local
    /// runtimes). Called by the strict config validation and by the
    /// adapter build (the runtime gate — a provider whose pricing config
    /// cannot be honored is never registered).
    pub fn validate_pricing(&self) -> Result<(), String> {
        match self.pricing() {
            Some(p) => p.validate(self.kind()),
            None => Ok(()),
        }
    }

    /// Config-load destination-class validation for a LITERAL base_url:
    /// a non-global literal endpoint is reachable only when the operator
    /// explicitly named it — either as an exact `[sandbox] network` rule
    /// for that destination or, for loopback, through this entry's
    /// `allow_loopback` rule (which also covers a NAME base_url that
    /// resolves onto loopback at connect time). Every other non-global
    /// literal is a typed startup error instead of a confusing runtime
    /// denial. Hostname base URLs cannot be judged without DNS and are
    /// enforced at connect time by the central resolver.
    pub(crate) fn validate_endpoint_address_class(
        &self,
        sandbox: Option<&faktor_security::destination::DestinationPolicy>,
    ) -> Result<(), String> {
        let Some(raw) = self.configured_base_url() else {
            return Ok(());
        };
        // Best-effort authority extraction: URL shape validation happens
        // where the adapter is built; this method only classifies a LITERAL
        // host, and anything that does not reduce to one falls through.
        let Some((scheme, rest)) = raw.split_once("://") else {
            return Ok(());
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port_text) = if let Some(bracketed) = authority.strip_prefix('[') {
            match bracketed.split_once(']') {
                Some((host, tail)) => (host, tail.strip_prefix(':')),
                None => return Ok(()),
            }
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        let Ok(ip) = host.parse::<std::net::IpAddr>() else {
            return Ok(());
        };
        let port: Option<u16> = port_text.and_then(|p| p.parse().ok());
        let class = faktor_security::network::classify_ip(ip);
        if class == faktor_security::network::AddressClass::Global {
            return Ok(());
        }
        // An exact allowlist entry for this destination is the operator
        // explicitly naming the literal, so it admits the destination.
        let (is_ipv4, octets) = match ip {
            std::net::IpAddr::V4(v4) => (true, Some(v4.octets())),
            std::net::IpAddr::V6(_) => (false, None),
        };
        // No installed allowlist (allow-all gate) admits every destination,
        // so a literal is admitted too.
        let Some(sandbox) = sandbox else {
            return Ok(());
        };
        if let Ok(target) = faktor_security::destination::RequestTarget::from_parts(
            Some(scheme),
            host,
            port,
            is_ipv4,
            octets,
        ) {
            if matches!(
                target.check_against(sandbox),
                faktor_security::destination::Decision::Allowed
            ) {
                return Ok(());
            }
        }
        if class == faktor_security::network::AddressClass::Loopback && self.allows_loopback() {
            return Ok(());
        }
        Err(format!(
            "base_url is a {class} address, which the egress address policy refuses unless \
             this entry sets `\"allow_loopback\": true` (loopback only) or the [sandbox] \
             network section allowlists that exact destination"
        ))
    }

    /// The configured key read from its env var (never stored in the file;
    /// the runtime never logs or persists the value). `None` when the entry
    /// carries no key env or the env var is unset. Wrapped in
    /// [`SecretValue`] (redacted `Debug`, zeroized on drop, explicit
    /// `expose()`); exposed for the daemon's outbound secret registry,
    /// which registers the SAME values the adapter builds from.
    pub(crate) fn key(&self) -> Option<SecretValue> {
        let env = match self {
            ProviderCfg::Ollama { .. } => return None,
            ProviderCfg::OpenAi { api_key_env, .. }
            | ProviderCfg::Anthropic { api_key_env, .. }
            | ProviderCfg::Google { api_key_env, .. }
            | ProviderCfg::DeepSeek { api_key_env, .. }
            | ProviderCfg::Gateway { api_key_env, .. } => api_key_env,
        };
        env.as_ref()
            .and_then(|name| std::env::var(name).ok())
            .map(SecretValue::new)
    }

    /// Build the adapter for this config entry over an explicit egress
    /// transport (the daemon passes the policy-checked transport built from
    /// its SandboxPolicy network gate + outbound secret scan; tests pass a
    /// default-allow one). Every provider is wrapped with its CONFIGURED
    /// instance id so the registry resolves by id (two OpenAI-compatible
    /// endpoints never overwrite each other; the adapter's family id stays
    /// for capability queries).
    /// Concrete Ollama provider when this entry configures one (the daemon
    /// warm-up keeps the concrete Arc so live probing reaches the SAME
    /// instance the registry serves).
    pub fn build_ollama(
        &self,
        transport: Arc<dyn HttpTransport>,
    ) -> Option<Arc<faktor_ollama::OllamaProvider>> {
        match self {
            ProviderCfg::Ollama { base_url, .. } => {
                let cfg = faktor_ollama::OllamaConfig::new(base_url.clone());
                Some(faktor_ollama::OllamaProvider::new(cfg, transport))
            }
            _ => None,
        }
    }

    pub fn build(&self, transport: Arc<dyn HttpTransport>) -> Result<Arc<dyn Provider>, String> {
        let provider: Arc<dyn Provider> = match self {
            ProviderCfg::Ollama { base_url, .. } => {
                let cfg = faktor_ollama::OllamaConfig::new(base_url.clone());
                faktor_ollama::OllamaProvider::new(cfg, transport.clone())
            }
            ProviderCfg::OpenAi { base_url, .. } => {
                let mut cfg = faktor_openai::OpenAiConfig::chat(base_url, self.key());
                cfg.family = self
                    .openai_family()
                    .expect("open_ai entries always select an OpenAI family");
                faktor_openai::OpenAiProvider::build(cfg, transport.clone())
            }
            ProviderCfg::Anthropic { .. } => {
                let cfg = faktor_anthropic::AnthropicConfig::new(self.key());
                faktor_anthropic::AnthropicProvider::build(cfg, transport.clone())
            }
            ProviderCfg::Google { .. } => {
                let cfg = faktor_google::GoogleConfig::new(self.key());
                faktor_google::GoogleProvider::build(cfg, transport.clone())
            }
            ProviderCfg::DeepSeek {
                profile, base_url, ..
            } => {
                let cfg = match profile.as_str() {
                    // The direct profile honors a configured base_url (a
                    // DeepSeek-compatible local/proxy endpoint): default is
                    // the native api.deepseek.com.
                    "direct" => {
                        let mut c = faktor_deepseek::DeepSeekConfig::direct(self.key());
                        if let Some(b) = base_url.clone() {
                            c.profile =
                                faktor_deepseek::DeepSeekProfile::Compatible { base_url: b };
                        }
                        c
                    }
                    // A gateway is BYO endpoint: there is no implicit
                    // third-party default, so a missing base_url is a loud
                    // typed refusal instead of a hardcoded vendor.
                    "gateway" => {
                        let Some(base_url) = base_url.clone() else {
                            return Err(
                                "deepseek profile \"gateway\" requires an explicit base_url \
                                 (no implicit gateway endpoint is assumed)"
                                    .into(),
                            );
                        };
                        faktor_deepseek::DeepSeekConfig {
                            profile: faktor_deepseek::DeepSeekProfile::Gateway { base_url },
                            api_key: self.key(),
                            model_overrides: Default::default(),
                        }
                    }
                    "openrouter" => faktor_deepseek::DeepSeekConfig {
                        profile: faktor_deepseek::DeepSeekProfile::OpenRouter,
                        api_key: self.key(),
                        model_overrides: Default::default(),
                    },
                    "compatible" => faktor_deepseek::DeepSeekConfig::compatible(
                        base_url
                            .clone()
                            .unwrap_or_else(|| "http://127.0.0.1:8000".into()),
                        self.key(),
                    ),
                    "local" => faktor_deepseek::DeepSeekConfig {
                        profile: faktor_deepseek::DeepSeekProfile::LocalDerivative {
                            base_url: base_url
                                .clone()
                                .unwrap_or_else(|| "http://127.0.0.1:8000".into()),
                        },
                        api_key: self.key(),
                        model_overrides: Default::default(),
                    },
                    other => {
                        return Err(format!("unknown deepseek profile {other:?}"));
                    }
                };
                faktor_deepseek::build(cfg, transport.clone())
            }
            ProviderCfg::Gateway { base_url, .. } => {
                let cfg = faktor_gateway::GatewayConfig {
                    id: "gateway".into(),
                    base_url: base_url.clone(),
                    api_key: self.key(),
                    extra_headers: faktor_provider::config::ExtraHeaders::empty(),
                    route_prefixes: vec![],
                    default_caps: ModelCapabilities::default(),
                };
                faktor_gateway::build(cfg, transport.clone())
            }
        };
        // Billing-origin audit + quality-authority declaration: every
        // configured endpoint is wrapped with its STRICTLY-resolved billing
        // origin (official canonical endpoint vs custom base_url vs gateway
        // vs local) and, when declared, its quality statement — applied to
        // THIS instance only. Validation happens here so a provider whose
        // pricing/quality cannot be honored never registers.
        //
        // A configured `pricing` section: exact prices -> UserOverride
        // rows, ceiling -> Composite rows for Unknown-priced models only;
        // both bump the pricing epoch. The `quality` section stamps
        // `UserConfigured` provenance and authorizes unknown-quality rows
        // (durable verified outcomes still supersede it). The
        // local-runtime (ollama) pricing gate also holds on the raw
        // `build` path (the daemon's warm-up path builds ollama separately,
        // where `Config::validate`/`load_strict` refuse such a config
        // loudly).
        let provider = self.wrap_catalog_authority(provider)?;
        Ok(provider)
    }

    /// Wrap an ALREADY-BUILT adapter with this entry's catalog authorities:
    /// the strictly-resolved billing origin, the validated pricing
    /// overrides and the validated quality declaration. The daemon's
    /// concrete-ollama path (live probing keeps the concrete Arc) uses this
    /// so the REGISTERED instance carries the same authority wrappers as
    /// [`ProviderCfg::build`].
    pub fn wrap_catalog_authority(
        &self,
        provider: Arc<dyn Provider>,
    ) -> Result<Arc<dyn Provider>, String> {
        let instance = self.id();
        let overrides = match self.pricing() {
            Some(pricing) => {
                pricing.validate(self.kind())?;
                pricing.to_overrides()
            }
            None => PricingOverrides::default(),
        };
        // Quality-authority declaration (item 1): validated HERE so a
        // provider whose quality cannot be honored never registers, then
        // applied only to THIS instance's catalog rows (the wrapper's
        // source names `providers.<id>.quality`). Durable verified outcomes
        // still supersede it at qualification.
        let quality = match self.quality() {
            Some(quality) => {
                quality.validate()?;
                quality.to_overrides()
            }
            None => faktor_provider::catalog::QualityOverrides::default(),
        };
        let origin_wrapped =
            BillingOriginProvider::wrap(provider, instance, self.billing_origin(), overrides);
        if quality.is_empty() {
            Ok(origin_wrapped)
        } else {
            Ok(QualityOverrideProvider::wrap(
                origin_wrapped,
                instance,
                quality,
            ))
        }
    }
}

#[cfg(test)]
#[path = "quality_declaration_tests.rs"]
mod quality_declaration_tests;
