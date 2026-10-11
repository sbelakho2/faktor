//! Vendored built-in list-price catalog (audit wave-B item C, billing-origin
//! audit, hard-budget-safe pricing audit, catalog-refresh item): documented
//! list prices of officially-known models ONLY — nothing invented, nothing
//! extrapolated.
//!
//! # The prices live in a checked-in vendored data file
//!
//! The rows are NOT hard-coded in Rust. They are transcribed in
//! `crates/provider/data/builtin_prices.json` (compiled in with
//! `include_str!`), which carries the table's provenance (`catalog_version`,
//! `source_id`, `source`), its dates (`effective_at_ms`, `observed_at_ms`,
//! `valid_until_ms`) and one `source` string per row. [`TABLE`] / [`table()`]
//! load and VALIDATE the file once at startup (a single lazy parse): an
//! unknown schema key, a non-official origin, a duplicate `(origin, model)`,
//! an empty model, a zero/omitted price line, an inconsistent date window or
//! an Anthropic row without its billed cache-write tariff is a hard failure —
//! the process refuses to serve a catalog it cannot trust instead of
//! fabricating a price. The unit test module re-parses the same vendored
//! bytes and drives the rejection matrix adversarially.
//!
//! Prices are microUSD per MILLION tokens (the published $/M reading x 1e6)
//! so sub-$1/M list prices ($0.15/M gpt-4o-mini input, $0.05/M gpt-5-nano
//! input, ...) stay exact and can never truncate to a free lie.
//!
//! Rows are keyed by [`BillingOrigin`] + model, NOT by transport family id:
//! a custom OpenAI-compatible proxy speaks the same wire protocol but bills
//! somewhere else, so it must never inherit official OpenAI list prices. The
//! legacy family-keyed [`lookup`] documents the raw-adapter compatibility
//! path (and is only reachable for adapters used OUTSIDE the config-built
//! daemon graph); the daemon resolves the real origin in config and uses
//! [`lookup_by_origin`].
//!
//! # Unknown models and retired rows
//!
//! A `(origin, model)` miss yields NO price at all: [`lookup_by_origin`]
//! returns `None` and callers stay [`PricingState::Unknown`] with the
//! [`QualityAuthority::ConservativeUnknown`] quality placeholder — never a
//! free/zero or 1-microUSD fabricated number. Rows whose official model ID
//! was RETIRED carry an explicit `retired_at_ms` in the file and are
//! excluded from every Exact lookup: a withdrawn ID never routes at its
//! last-known price.
//!
//! # builtin-v3: metadata, validity windows and conditional tariffs
//!
//! Every row carries `effective_at_ms` / `observed_at_ms` /
//! `valid_until_ms` (file defaults unless the row overrides them),
//! `source_id` and `catalog_version` ([`BUILTIN_CATALOG_VERSION`] =
//! `"builtin-v3"`, [`BUILTIN_SOURCE_ID`] = `"faktor-builtin-v3"`): a price
//! statement is dated, and a quote past `valid_until_ms` stops being exact
//! at route time (see `PricingState::effective_at`).
//!
//! Tariffs are conditional, not a single number:
//!
//! - Anthropic charges prompt-cache WRITES, and the write rate depends on
//!   the cache TTL: [`BuiltinCacheWrite::TtlDependent`] carries the 5-minute
//!   and 1-hour rates, and [`BuiltinRowSchedule`] — a [`PriceSchedule`] —
//!   applies the known TTL exactly, or the MAXIMUM of the two for a hard
//!   budget whose applicable tariff is unknown (never the minimum);
//! - cache-read rows are included where documented; `cache_write` is never
//!   zero-by-omission on a provider that bills writes (the loader refuses an
//!   Anthropic row without a nonzero TTL-dependent write tariff);
//! - DeepSeek moved to V4-era schedules the audited table does not document
//!   as exact: [`BuiltinPriceClass::ConservativeSchedule`] marks a row whose
//!   numbers are a documented CONSERVATIVE BOUND (a ceiling), never a
//!   claimed exact list price.
//!
//! # Performance profiles
//!
//! [`performance_prior`] exposes the documented FAKTOR ROUTING PRIORS for
//! the same models — reliability/latency estimates the economic router
//! uses to compare candidates. These are deliberate, conservative routing
//! priors, NOT vendor truths: every profile carries
//! [`QualityAuthority::BuiltInPrior`] and the
//! [`PERFORMANCE_PRIOR_VERSION`] benchmark string, and durable verified
//! outcomes dominate them whenever verified history exists.

use std::sync::LazyLock;

use faktor_core::model::{
    unix_now_ms, BillingOrigin, CacheWriteTtl, MicroUsdPerMillionTokens, ModelPerformance,
    ModelPerformanceProfile, PriceAuthority, PriceQuote, PriceSchedule, PriceUnavailable,
    PricingContext, PricingSnapshot, PricingState, QualityAuthority, ServiceTier,
};

use crate::catalog::PricingProvenance;

/// Version of the loaded built-in table (must equal the vendored data file's
/// `catalog_version`; the loader refuses a mismatch, so old snapshots never
/// claim a new version).
pub const BUILTIN_CATALOG_VERSION: &str = "builtin-v3";

/// Source id stamped on every built-in row's snapshot (must equal the
/// vendored data file's `source_id`).
pub const BUILTIN_SOURCE_ID: &str = "faktor-builtin-v3";

/// The vendored catalog file the table is loaded from (compiled in at build
/// time). Path is repo-relative for documentation; the loader reads the
/// compile-time contents.
pub const BUILTIN_CATALOG_FILE: &str = "crates/provider/data/builtin_prices.json";

/// The compile-time contents of the vendored catalog file.
const BUILTIN_CATALOG_JSON: &str = include_str!("../../data/builtin_prices.json");

/// The file-level provenance and date window of the vendored catalog: who
/// published the prices, when they took effect, when they were observed and
/// until when they stay authoritative. A row may override the dates (a
/// retired row's window ends at its retirement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinCatalogMeta {
    /// Catalog version string (`"builtin-v3"`).
    pub catalog_version: &'static str,
    /// Source id stamped on snapshots (`"faktor-builtin-v3"`).
    pub source_id: &'static str,
    /// Free-text provenance of the whole file (per-row URLs live on each
    /// row's `source`).
    pub source: &'static str,
    /// Wall-clock ms the file's prices took effect.
    pub effective_at_ms: u64,
    /// Wall-clock ms the file's prices were observed.
    pub observed_at_ms: u64,
    /// Wall-clock ms after which the file's exact quotes stop being
    /// authoritative (a row's override can end earlier).
    pub valid_until_ms: u64,
}

/// How a row's cache-write line is billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BuiltinCacheWrite {
    /// The provider publishes no cache-write charge.
    NotBilled,
    /// One published write rate (microUSD per million tokens).
    Fixed { rate: u64 },
    /// TTL-dependent published write rates (the Anthropic shape).
    TtlDependent { five_minutes: u64, one_hour: u64 },
}

impl BuiltinCacheWrite {
    /// The maximum documented write rate: the conservative tariff when the
    /// applicable TTL is unknown.
    pub const fn max_rate(self) -> u64 {
        match self {
            BuiltinCacheWrite::NotBilled => 0,
            BuiltinCacheWrite::Fixed { rate } => rate,
            BuiltinCacheWrite::TtlDependent {
                five_minutes,
                one_hour,
            } => {
                if five_minutes > one_hour {
                    five_minutes
                } else {
                    one_hour
                }
            }
        }
    }

    /// The write rate for a KNOWN TTL, or `None` when that tariff does not
    /// exist / the tariff is not known and `hard_budget` is false.
    pub const fn rate_for(self, ttl: Option<CacheWriteTtl>, hard_budget: bool) -> Option<u64> {
        match self {
            BuiltinCacheWrite::NotBilled => Some(0),
            BuiltinCacheWrite::Fixed { rate } => Some(rate),
            BuiltinCacheWrite::TtlDependent {
                five_minutes,
                one_hour,
            } => match ttl {
                Some(CacheWriteTtl::FiveMinutes) => Some(five_minutes),
                Some(CacheWriteTtl::OneHour) => Some(one_hour),
                None if hard_budget => Some(if five_minutes > one_hour {
                    five_minutes
                } else {
                    one_hour
                }),
                None => None,
            },
        }
    }

    /// Every documented rate is nonzero on a row that bills writes (the
    /// loader's zero-by-omission refusal).
    const fn has_nonzero_rates(self) -> bool {
        match self {
            BuiltinCacheWrite::NotBilled => true,
            BuiltinCacheWrite::Fixed { rate } => rate > 0,
            BuiltinCacheWrite::TtlDependent {
                five_minutes,
                one_hour,
            } => five_minutes > 0 && one_hour > 0,
        }
    }
}

/// What a row's numbers REST on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinPriceClass {
    /// Documented current list prices (exact statement with a validity
    /// window).
    Exact,
    /// No exact V4-era facts are documented: the numbers are a documented
    /// CONSERVATIVE BOUND and the resolved state is
    /// [`PricingState::ConservativeCeiling`], admissible under a hard cap
    /// but never claimed as a measured price.
    ConservativeSchedule,
}

/// One documented list-price row (per-million-token microUSD) with its
/// catalog metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinPrice {
    /// The BILLING origin the row belongs to (never a transport family id).
    pub origin: BillingOrigin,
    /// The officially-known model name (matched case-insensitively).
    pub model: &'static str,
    /// What the numeric lines rest on.
    pub class: BuiltinPriceClass,
    pub input_micro_usd_per_million: u64,
    pub output_micro_usd_per_million: u64,
    /// Published cache-read line, when the audit records one; `None`
    /// otherwise (the line is 0 — never billed).
    pub cache_read_micro_usd_per_million: Option<u64>,
    /// Published cache-write tariff(s); never silently zero on a provider
    /// that bills writes.
    pub cache_write: BuiltinCacheWrite,
    /// Wall-clock ms the row's prices took effect.
    pub effective_at_ms: u64,
    /// Wall-clock ms the row's prices were observed.
    pub observed_at_ms: u64,
    /// Wall-clock ms after which the exact quote stops being authoritative
    /// at route time (`None` = no published expiry).
    pub valid_until_ms: Option<u64>,
    /// Wall-clock ms the official model ID was retired (`None` = active).
    /// Retired rows are documented here but excluded from every Exact
    /// lookup: a withdrawn ID never routes at its last-known price.
    pub retired_at_ms: Option<u64>,
    /// Source id stamped on snapshots cut from this row.
    pub source_id: &'static str,
    /// Catalog version of this row.
    pub catalog_version: &'static str,
    /// The row's own provenance: the vendor/source URL or the documented
    /// bound note the numbers are transcribed from.
    pub source: &'static str,
}

impl BuiltinPrice {
    /// True once the official model ID was retired.
    pub const fn is_retired(&self) -> bool {
        self.retired_at_ms.is_some()
    }

    /// True once the validity window elapsed at `at_ms` (inclusive
    /// boundary). A row without a window never expires.
    pub const fn is_expired_at(&self, at_ms: u64) -> bool {
        match self.valid_until_ms {
            Some(until) => at_ms >= until,
            None => false,
        }
    }
}

// ------------------------------------------------------------------- loader

/// One catalog row exactly as transcribed in the vendored file. Unknown
/// keys are rejected so a typo (`input_micro_usd_per_millionn`, ...) can
/// never be silently ignored into a default.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogRowFile {
    origin: BillingOrigin,
    model: String,
    class: BuiltinPriceClass,
    input_micro_usd_per_million: u64,
    output_micro_usd_per_million: u64,
    #[serde(default)]
    cache_read_micro_usd_per_million: Option<u64>,
    cache_write: BuiltinCacheWrite,
    /// The row's own provenance (vendor pricing URL or bound note).
    source: String,
    #[serde(default)]
    effective_at_ms: Option<u64>,
    #[serde(default)]
    observed_at_ms: Option<u64>,
    #[serde(default)]
    valid_until_ms: Option<u64>,
    #[serde(default)]
    retired_at_ms: Option<u64>,
}

/// The vendored file's schema: file-level provenance + dates, then rows.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    catalog_version: String,
    source_id: String,
    source: String,
    effective_at_ms: u64,
    observed_at_ms: u64,
    valid_until_ms: u64,
    rows: Vec<CatalogRowFile>,
}

/// One parsed-and-validated catalog generation: the file's provenance plus
/// the row table.
#[derive(Debug)]
struct LoadedCatalog {
    meta: BuiltinCatalogMeta,
    rows: Vec<BuiltinPrice>,
}

/// Leak a parsed string into the `&'static str` a [`BuiltinPrice`] carries.
/// Bounded one-time cost: the loader runs once per process for the vendored
/// table (and a handful of times in the validation tests).
fn leak(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

/// Parse and VALIDATE the vendored catalog bytes. Every failure is a typed
/// string naming the offending row; a production call site turns it into a
/// hard startup failure, never a partial/zero-priced table.
fn load_catalog(json: &str) -> Result<LoadedCatalog, String> {
    let file: CatalogFile =
        serde_json::from_str(json).map_err(|e| format!("catalog JSON does not parse: {e}"))?;
    if file.catalog_version != BUILTIN_CATALOG_VERSION {
        return Err(format!(
            "catalog_version {:?} does not match {BUILTIN_CATALOG_VERSION:?}",
            file.catalog_version
        ));
    }
    if file.source_id != BUILTIN_SOURCE_ID {
        return Err(format!(
            "source_id {:?} does not match {BUILTIN_SOURCE_ID:?}",
            file.source_id
        ));
    }
    if file.source.trim().is_empty() {
        return Err("file-level source must name where the prices came from".to_string());
    }
    if file.effective_at_ms == 0
        || file.observed_at_ms < file.effective_at_ms
        || file.valid_until_ms <= file.observed_at_ms
    {
        return Err(format!(
            "file date window is inconsistent: effective={} observed={} valid_until={}",
            file.effective_at_ms, file.observed_at_ms, file.valid_until_ms
        ));
    }
    if file.rows.is_empty() {
        return Err("catalog must document at least one row".to_string());
    }
    let mut rows: Vec<BuiltinPrice> = Vec::with_capacity(file.rows.len());
    let mut seen: Vec<(BillingOrigin, String)> = Vec::with_capacity(file.rows.len());
    for row in &file.rows {
        let where_ = format!("{}:{}", origin_label(row.origin), row.model);
        if !row.origin.is_official() {
            return Err(format!(
                "{where_}: built-in rows must name an OFFICIAL billing origin"
            ));
        }
        let model = row.model.trim();
        if model.is_empty() || model.chars().any(char::is_whitespace) {
            return Err(format!("{where_}: model must be a non-empty single token"));
        }
        if row.input_micro_usd_per_million == 0 || row.output_micro_usd_per_million == 0 {
            return Err(format!(
                "{where_}: input/output list prices must be nonzero (a zero reading is a \
                 transcription failure, never a free model)"
            ));
        }
        if row.cache_read_micro_usd_per_million == Some(0) {
            return Err(format!(
                "{where_}: a published cache-read line of zero must be omitted, not recorded"
            ));
        }
        if !row.cache_write.has_nonzero_rates() {
            return Err(format!(
                "{where_}: a billed cache-write tariff must be nonzero"
            ));
        }
        if row.origin == BillingOrigin::OfficialAnthropic
            && !matches!(row.cache_write, BuiltinCacheWrite::TtlDependent { .. })
        {
            return Err(format!(
                "{where_}: Anthropic bills cache writes, so its rows must carry the \
                 TTL-dependent write tariff"
            ));
        }
        if row.origin == BillingOrigin::OfficialAnthropic
            && row.cache_read_micro_usd_per_million.is_none()
        {
            return Err(format!(
                "{where_}: Anthropic documents cache reads (0.1x input); the line must be \
                 recorded"
            ));
        }
        let model_key = model.to_ascii_lowercase();
        if seen
            .iter()
            .any(|(origin, existing)| *origin == row.origin && *existing == model_key)
        {
            return Err(format!("{where_}: duplicate (origin, model) row"));
        }
        if row.source.trim().is_empty() {
            return Err(format!(
                "{where_}: row source must name where the price came from"
            ));
        }
        let effective_at_ms = row.effective_at_ms.unwrap_or(file.effective_at_ms);
        let observed_at_ms = row.observed_at_ms.unwrap_or(file.observed_at_ms);
        let valid_until_ms = row.valid_until_ms.unwrap_or(file.valid_until_ms);
        if observed_at_ms < effective_at_ms || valid_until_ms <= effective_at_ms {
            return Err(format!(
                "{where_}: date window is inconsistent: effective={effective_at_ms} \
                 observed={observed_at_ms} valid_until={valid_until_ms}"
            ));
        }
        match row.retired_at_ms {
            Some(retired_at_ms) => {
                if retired_at_ms < effective_at_ms || retired_at_ms > valid_until_ms {
                    return Err(format!(
                        "{where_}: retired_at_ms={retired_at_ms} outside the row's \
                         [effective, valid_until] window"
                    ));
                }
            }
            // An ACTIVE row's window must still be open at observation time.
            None if valid_until_ms <= observed_at_ms => {
                return Err(format!(
                    "{where_}: an active row cannot already be expired at observation time \
                     (mark it retired explicitly instead)"
                ));
            }
            None => {}
        }
        seen.push((row.origin, model_key));
        rows.push(BuiltinPrice {
            origin: row.origin,
            model: leak(model.to_string()),
            class: row.class,
            input_micro_usd_per_million: row.input_micro_usd_per_million,
            output_micro_usd_per_million: row.output_micro_usd_per_million,
            cache_read_micro_usd_per_million: row.cache_read_micro_usd_per_million,
            cache_write: row.cache_write,
            effective_at_ms,
            observed_at_ms,
            valid_until_ms: Some(valid_until_ms),
            retired_at_ms: row.retired_at_ms,
            source_id: BUILTIN_SOURCE_ID,
            catalog_version: BUILTIN_CATALOG_VERSION,
            source: leak(row.source.trim().to_string()),
        });
    }
    // Every official origin must keep at least one ACTIVE documented row:
    // a refresh that retires an origin's last row would silently strip the
    // origin of all exact pricing.
    for origin in [
        BillingOrigin::OfficialOpenAi,
        BillingOrigin::OfficialAnthropic,
        BillingOrigin::OfficialGoogle,
        BillingOrigin::OfficialDeepSeek,
    ] {
        if !rows
            .iter()
            .any(|row| row.origin == origin && !row.is_retired())
        {
            return Err(format!(
                "{}: no active documented row remains",
                origin_label(origin)
            ));
        }
    }
    Ok(LoadedCatalog {
        meta: BuiltinCatalogMeta {
            catalog_version: BUILTIN_CATALOG_VERSION,
            source_id: BUILTIN_SOURCE_ID,
            source: leak(file.source.trim().to_string()),
            effective_at_ms: file.effective_at_ms,
            observed_at_ms: file.observed_at_ms,
            valid_until_ms: file.valid_until_ms,
        },
        rows,
    })
}

/// A stable human label for one origin (error messages only).
fn origin_label(origin: BillingOrigin) -> &'static str {
    match origin {
        BillingOrigin::OfficialOpenAi => "openai",
        BillingOrigin::OfficialAnthropic => "anthropic",
        BillingOrigin::OfficialGoogle => "google",
        BillingOrigin::OfficialDeepSeek => "deepseek",
        BillingOrigin::CustomEndpoint => "custom-endpoint",
        BillingOrigin::Gateway => "gateway",
        BillingOrigin::Local => "local",
    }
}

/// The one loaded catalog generation (the vendored file parsed and
/// validated exactly once). A failure is a hard process-startup refusal:
/// serving Unknown prices because the shipped table is corrupt would hide a
/// real configuration defect.
static CATALOG: LazyLock<LoadedCatalog> = LazyLock::new(|| {
    load_catalog(BUILTIN_CATALOG_JSON).unwrap_or_else(|error| {
        panic!("vendored builtin price catalog ({BUILTIN_CATALOG_FILE}) is invalid: {error}")
    })
});

/// The frozen, versioned price table (the vendored file's validated rows).
pub static TABLE: LazyLock<Vec<BuiltinPrice>> = LazyLock::new(|| CATALOG.rows.clone());

/// The frozen, versioned price table as a slice.
pub fn table() -> &'static [BuiltinPrice] {
    &CATALOG.rows
}

/// The file-level provenance and date window of the loaded table.
pub fn meta() -> &'static BuiltinCatalogMeta {
    &CATALOG.meta
}

/// The documented effective date of the loaded rows.
pub fn effective_at_ms() -> u64 {
    meta().effective_at_ms
}

/// The documented observation date of the loaded rows.
pub fn observed_at_ms() -> u64 {
    meta().observed_at_ms
}

/// The validity window of the loaded rows (row overrides can end earlier).
pub fn valid_until_ms() -> u64 {
    meta().valid_until_ms
}

/// One documented FAKTOR ROUTING PRIOR row (not a vendor claim).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinPerformance {
    pub origin: BillingOrigin,
    pub model: &'static str,
    pub context_reliability: u8,
    pub coding_reliability: u8,
    pub estimated_latency_ms: u64,
}

/// Version string of the built-in Faktor routing priors (the performance
/// profiles below). Bump whenever a prior changes.
pub const PERFORMANCE_PRIOR_VERSION: &str = "faktor-routing-priors-v2";

/// Source id of the built-in performance priors (inspectable identity of a
/// conservative-unknown prior).
pub const PERFORMANCE_PRIOR_SOURCE_ID: &str = "faktor-builtin-priors-v2";

/// The documented Faktor routing priors. Values are conservative relative
/// ordering estimates: frontier coding models sit in the high 80s/90s, small
/// flash/mini models lower, with latency roughly anti-correlated. They exist
/// so a fresh Balanced configuration has a real candidate above its default
/// quality floor instead of silently failing every route; durable verified
/// outcomes replace them per (provider, model, phase) once recorded.
pub const PERFORMANCE_TABLE: &[BuiltinPerformance] = &[
    BuiltinPerformance {
        origin: BillingOrigin::OfficialOpenAi,
        model: "gpt-4o",
        context_reliability: 90,
        coding_reliability: 90,
        estimated_latency_ms: 700,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialOpenAi,
        model: "gpt-4o-mini",
        context_reliability: 78,
        coding_reliability: 74,
        estimated_latency_ms: 500,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialOpenAi,
        model: "o3",
        context_reliability: 92,
        coding_reliability: 94,
        estimated_latency_ms: 2_000,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialOpenAi,
        model: "o4-mini",
        context_reliability: 86,
        coding_reliability: 88,
        estimated_latency_ms: 1_200,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialOpenAi,
        model: "gpt-5",
        context_reliability: 94,
        coding_reliability: 95,
        estimated_latency_ms: 1_500,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialOpenAi,
        model: "gpt-5-mini",
        context_reliability: 86,
        coding_reliability: 85,
        estimated_latency_ms: 800,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialOpenAi,
        model: "gpt-5-nano",
        context_reliability: 74,
        coding_reliability: 70,
        estimated_latency_ms: 400,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialAnthropic,
        model: "claude-opus-4",
        context_reliability: 92,
        coding_reliability: 95,
        estimated_latency_ms: 1_500,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialAnthropic,
        model: "claude-opus-4-1",
        context_reliability: 93,
        coding_reliability: 96,
        estimated_latency_ms: 1_600,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialAnthropic,
        model: "claude-sonnet-4-5",
        context_reliability: 93,
        coding_reliability: 94,
        estimated_latency_ms: 900,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialAnthropic,
        model: "claude-haiku-4-5",
        context_reliability: 84,
        coding_reliability: 80,
        estimated_latency_ms: 450,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialAnthropic,
        model: "claude-sonnet-4",
        context_reliability: 92,
        coding_reliability: 93,
        estimated_latency_ms: 900,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialAnthropic,
        model: "claude-haiku-3.5",
        context_reliability: 82,
        coding_reliability: 78,
        estimated_latency_ms: 450,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialGoogle,
        model: "gemini-2.0-flash",
        context_reliability: 88,
        coding_reliability: 84,
        estimated_latency_ms: 350,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialGoogle,
        model: "gemini-2.5-flash",
        context_reliability: 88,
        coding_reliability: 84,
        estimated_latency_ms: 300,
    },
    BuiltinPerformance {
        origin: BillingOrigin::OfficialDeepSeek,
        model: "deepseek-chat",
        context_reliability: 88,
        coding_reliability: 90,
        estimated_latency_ms: 1_100,
    },
];

/// The provenance of the whole loaded table (the file's own dates).
pub fn provenance() -> PricingProvenance {
    PricingProvenance {
        source_id: meta().source_id.to_string(),
        effective_at_ms: Some(meta().effective_at_ms),
        observed_at_ms: Some(meta().observed_at_ms),
        valid_until_ms: Some(meta().valid_until_ms),
        catalog_version: meta().catalog_version.to_string(),
    }
}

/// The provenance of ONE row (every row carries its own dates, source id
/// and catalog version).
pub fn provenance_of(row: &BuiltinPrice) -> PricingProvenance {
    PricingProvenance {
        source_id: row.source_id.to_string(),
        effective_at_ms: Some(row.effective_at_ms),
        observed_at_ms: Some(row.observed_at_ms),
        valid_until_ms: row.valid_until_ms,
        catalog_version: row.catalog_version.to_string(),
    }
}

/// The ROW as a conditional [`PriceSchedule`]: applies the documented
/// validity window, service tier and cache-write TTL (unknown TTL + hard
/// budget → the maximum documented write tariff). Conservative-schedule
/// rows quote with [`PriceAuthority::ConservativeCeiling`] authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinRowSchedule {
    pub row: BuiltinPrice,
    pub epoch: u64,
}

impl BuiltinRowSchedule {
    pub const fn new(row: BuiltinPrice, epoch: u64) -> Self {
        Self { row, epoch }
    }

    fn priced_quote(
        &self,
        ctx: &PricingContext,
        hard_budget: bool,
    ) -> Result<(PriceQuote, PriceAuthority), PriceUnavailable> {
        let row = &self.row;
        if row.is_retired() {
            return Err(PriceUnavailable::Retired);
        }
        if ctx.at_ms < row.effective_at_ms {
            // The row is not yet in force: no applicable tariff.
            return Err(PriceUnavailable::NoApplicableTariff);
        }
        if row.is_expired_at(ctx.at_ms) {
            return Err(PriceUnavailable::Expired);
        }
        if let Some(tier) = ctx.service_tier {
            if tier != ServiceTier::Standard {
                return Err(PriceUnavailable::NoApplicableTariff);
            }
        }
        let Some(cache_write) = row.cache_write.rate_for(ctx.cache_write_ttl, hard_budget) else {
            return Err(PriceUnavailable::NoApplicableTariff);
        };
        let quote = PriceQuote {
            input: MicroUsdPerMillionTokens(row.input_micro_usd_per_million),
            output: MicroUsdPerMillionTokens(row.output_micro_usd_per_million),
            cache_read: MicroUsdPerMillionTokens(row.cache_read_micro_usd_per_million.unwrap_or(0)),
            cache_write: MicroUsdPerMillionTokens(cache_write),
        };
        let authority = match row.class {
            BuiltinPriceClass::Exact => PriceAuthority::Exact,
            BuiltinPriceClass::ConservativeSchedule => PriceAuthority::ConservativeCeiling,
        };
        Ok((quote, authority))
    }

    fn snapshot(&self, quote: PriceQuote, authority: PriceAuthority) -> PricingSnapshot {
        let snap = match authority {
            PriceAuthority::ConservativeCeiling => PricingSnapshot::conservative_ceiling(
                quote,
                self.epoch,
                self.row.source_id.to_string(),
            ),
            _ => PricingSnapshot::exact(quote, self.epoch, self.row.source_id.to_string()),
        };
        match self.row.valid_until_ms {
            Some(until) => snap.with_valid_until(until),
            None => snap,
        }
    }
}

impl PriceSchedule for BuiltinRowSchedule {
    fn quote(&self, ctx: &PricingContext) -> Result<PricingSnapshot, PriceUnavailable> {
        let (quote, authority) = self.priced_quote(ctx, false)?;
        Ok(self.snapshot(quote, authority))
    }

    fn quote_for_hard_budget(
        &self,
        ctx: &PricingContext,
    ) -> Result<PricingSnapshot, PriceUnavailable> {
        let (quote, authority) = self.priced_quote(ctx, true)?;
        Ok(self.snapshot(quote, authority))
    }
}

/// Exact list-price lookup keyed by BILLING ORIGIN + model (the production
/// path): case-insensitive on the model, exact on the origin, and RETIRED
/// rows excluded. `None` for anything not in the documented table — callers
/// then stay [`PricingState::Unknown`].
pub fn lookup_by_origin(origin: BillingOrigin, model: &str) -> Option<BuiltinPrice> {
    lookup_documented_by_origin(origin, model).filter(|row| !row.is_retired())
}

/// Documented-row lookup INCLUDING retired rows (metadata/tests only; the
/// routing paths use [`lookup_by_origin`]).
pub fn lookup_documented_by_origin(origin: BillingOrigin, model: &str) -> Option<BuiltinPrice> {
    let model = model.trim().to_ascii_lowercase();
    table()
        .iter()
        .copied()
        .find(|row| row.origin == origin && row.model.to_ascii_lowercase() == model)
}

/// LEGACY family-keyed lookup kept for the raw `Provider::catalog_entry`
/// trait default (untouched adapters construct rows without config): maps the
/// canonical adapter family names onto their official billing origins and
/// otherwise misses. Config-built daemon providers NEVER use this — they
/// resolve [`BillingOrigin`] strictly and call [`lookup_by_origin`], so a
/// custom OpenAI-compatible endpoint can never inherit official prices.
///
/// The raw trait default can only build an EXACT row, so this shim also
/// excludes conservative-schedule and retired rows: those must resolve
/// through the billing-origin wrapper (which understands the pricing state).
pub fn lookup(family: &str, model: &str) -> Option<BuiltinPrice> {
    family_origin(family)
        .and_then(|origin| lookup_by_origin(origin, model))
        .filter(|row| row.class == BuiltinPriceClass::Exact)
}

fn family_origin(family: &str) -> Option<BillingOrigin> {
    match family.trim().to_ascii_lowercase().as_str() {
        "openai" => Some(BillingOrigin::OfficialOpenAi),
        "anthropic" => Some(BillingOrigin::OfficialAnthropic),
        "google" => Some(BillingOrigin::OfficialGoogle),
        "deepseek" => Some(BillingOrigin::OfficialDeepSeek),
        _ => None,
    }
}

/// Resolve one documented row into its route-time [`PricingState`] at
/// `at_ms`, using the HARD-BUDGET tariff selection (unknown TTL → maximum
/// documented write tariff) so the catalog never under-reserves:
///
/// - [`BuiltinPriceClass::Exact`] → `Known` at the max applicable tariff,
///   with the row's validity window attached;
/// - [`BuiltinPriceClass::ConservativeSchedule`] → `ConservativeCeiling`
///   (a documented bound, never a claimed exact price);
/// - retired/expired/not-yet-effective rows → `Unknown`.
pub fn pricing_state_at(row: &BuiltinPrice, at_ms: u64) -> PricingState {
    let schedule = BuiltinRowSchedule::new(*row, crate::catalog::CATALOG_FIRST_EPOCH);
    let ctx = PricingContext::at(at_ms);
    match schedule.quote_for_hard_budget(&ctx) {
        Ok(snapshot) => match snapshot.authority {
            PriceAuthority::Exact => PricingState::Known(snapshot),
            PriceAuthority::ConservativeCeiling => PricingState::ConservativeCeiling(snapshot),
            _ => PricingState::Unknown,
        },
        Err(_) => PricingState::Unknown,
    }
}

/// [`pricing_state_at`] at the current wall clock (the catalog resolution
/// path).
pub fn pricing_state_of(row: &BuiltinPrice) -> PricingState {
    pricing_state_at(row, unix_now_ms())
}

/// The exact per-million-token quote of one built-in row, with the
/// conservative max cache-write tariff (`cache_write` is max(TTL tariffs)
/// because the raw compatibility snapshot carries no context). DeepSeek
/// rows are conservative bounds; callers that need the authority use
/// [`pricing_state_at`].
pub fn quote_of(row: &BuiltinPrice) -> PriceQuote {
    PriceQuote {
        input: MicroUsdPerMillionTokens(row.input_micro_usd_per_million),
        output: MicroUsdPerMillionTokens(row.output_micro_usd_per_million),
        cache_read: MicroUsdPerMillionTokens(row.cache_read_micro_usd_per_million.unwrap_or(0)),
        cache_write: MicroUsdPerMillionTokens(row.cache_write.max_rate()),
    }
}

/// The documented Faktor routing prior of one (origin, model), when the
/// table documents it: a [`ModelPerformanceProfile`] carrying the explicit
/// built-in-prior authority and the frozen benchmark version.
pub fn performance_prior(origin: BillingOrigin, model: &str) -> Option<ModelPerformanceProfile> {
    let model = model.trim().to_ascii_lowercase();
    PERFORMANCE_TABLE
        .iter()
        .copied()
        .find(|row| row.origin == origin && row.model.to_ascii_lowercase() == model)
        .map(|row| ModelPerformanceProfile {
            prior: ModelPerformance {
                context_reliability: row.context_reliability,
                coding_reliability: row.coding_reliability,
                estimated_latency_ms: row.estimated_latency_ms,
                rate_limit_state: faktor_core::model::RateLimitState::Healthy,
            },
            authority: QualityAuthority::BuiltInPrior,
            benchmark_version: PERFORMANCE_PRIOR_VERSION.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored file is the source of truth: the loaded table must
    /// match it byte-for-row, and the file must carry its provenance, dates
    /// and per-row sources.
    #[test]
    fn vendored_file_loads_with_provenance_dates_and_per_row_sources() {
        let file: serde_json::Value =
            serde_json::from_str(BUILTIN_CATALOG_JSON).expect("vendored catalog parses");
        assert_eq!(file["catalog_version"], BUILTIN_CATALOG_VERSION);
        assert_eq!(file["source_id"], BUILTIN_SOURCE_ID);
        assert!(
            file["source"].as_str().expect("file source").len() > 40,
            "file-level source must describe the provenance"
        );
        assert_eq!(
            meta().effective_at_ms,
            file["effective_at_ms"].as_u64().unwrap()
        );
        assert_eq!(
            meta().observed_at_ms,
            file["observed_at_ms"].as_u64().unwrap()
        );
        assert_eq!(
            meta().valid_until_ms,
            file["valid_until_ms"].as_u64().unwrap()
        );
        assert!(meta().effective_at_ms < meta().observed_at_ms);
        assert!(meta().observed_at_ms < meta().valid_until_ms);
        assert_eq!(table().len(), TABLE.len());
        let file_rows = file["rows"].as_array().expect("rows array");
        assert_eq!(table().len(), file_rows.len(), "loaded count == file count");
        for (loaded, raw) in table().iter().zip(file_rows) {
            assert_eq!(loaded.model, raw["model"].as_str().unwrap());
            assert_eq!(
                loaded.input_micro_usd_per_million,
                raw["input_micro_usd_per_million"].as_u64().unwrap()
            );
            assert_eq!(
                loaded.output_micro_usd_per_million,
                raw["output_micro_usd_per_million"].as_u64().unwrap()
            );
            assert_eq!(
                loaded.cache_read_micro_usd_per_million,
                raw["cache_read_micro_usd_per_million"].as_u64()
            );
            assert_eq!(loaded.retired_at_ms, raw["retired_at_ms"].as_u64());
            assert!(loaded.origin.is_official(), "official origin only");
            assert!(
                !loaded.source.trim().is_empty(),
                "{} row source",
                loaded.model
            );
            assert!(loaded.source.contains("http") || loaded.source.contains("bound"));
            assert!(loaded.input_micro_usd_per_million > 0);
            assert!(loaded.output_micro_usd_per_million > 0);
            assert_eq!(loaded.source_id, BUILTIN_SOURCE_ID);
            assert_eq!(loaded.catalog_version, BUILTIN_CATALOG_VERSION);
        }
    }

    /// The loader's rejection matrix: a corrupted vendored file must NEVER
    /// load as a partial/zero-priced table. Each mutation is applied to the
    /// real checked-in bytes.
    #[test]
    fn loader_rejects_corrupted_catalog_data_adversarially() {
        let base: serde_json::Value =
            serde_json::from_str(BUILTIN_CATALOG_JSON).expect("vendored catalog parses");
        let mutate = |f: &dyn Fn(&mut serde_json::Value)| -> String {
            let mut copy = base.clone();
            f(&mut copy);
            load_catalog(&copy.to_string()).expect_err("corrupt catalog must not load")
        };

        // Unknown key: a typo can never be silently ignored.
        let err = mutate(&|doc| {
            doc["rows"][0]["input_micro_usd_per_millionn"] = serde_json::json!(1);
        });
        assert!(err.contains("input_micro_usd_per_millionn"), "{err}");
        // Zero input price (the free-model lie).
        let err = mutate(&|doc| {
            doc["rows"][0]["input_micro_usd_per_million"] = serde_json::json!(0);
        });
        assert!(err.contains("nonzero"), "{err}");
        // Zero output price.
        let err = mutate(&|doc| {
            doc["rows"][0]["output_micro_usd_per_million"] = serde_json::json!(0);
        });
        assert!(err.contains("nonzero"), "{err}");
        // A recorded cache-read zero instead of an omitted line.
        let err = mutate(&|doc| {
            doc["rows"][0]["cache_read_micro_usd_per_million"] = serde_json::json!(0);
        });
        assert!(err.contains("cache-read"), "{err}");
        // A duplicate (origin, model) row.
        let err = mutate(&|doc| {
            let row = doc["rows"][0].clone();
            doc["rows"].as_array_mut().unwrap().push(row);
        });
        assert!(err.contains("duplicate"), "{err}");
        // A non-official origin.
        let err = mutate(&|doc| {
            doc["rows"][0]["origin"] = serde_json::json!("custom_endpoint");
        });
        assert!(err.contains("OFFICIAL"), "{err}");
        // A retired row without the explicit retirement date (its window is
        // closed at observation time, so it cannot pass as active).
        let err = mutate(&|doc| {
            for row in doc["rows"].as_array_mut().unwrap() {
                if row["model"] == "claude-sonnet-4" {
                    row.as_object_mut().unwrap().remove("retired_at_ms");
                }
            }
        });
        assert!(err.contains("retired") || err.contains("expired"), "{err}");
        // A retirement date outside the row's window.
        let err = mutate(&|doc| {
            for row in doc["rows"].as_array_mut().unwrap() {
                if row["model"] == "gpt-4o" {
                    row["retired_at_ms"] = serde_json::json!(u64::MAX);
                }
            }
        });
        assert!(err.contains("retired_at_ms"), "{err}");
        // A catalog-version drift.
        let err = mutate(&|doc| {
            doc["catalog_version"] = serde_json::json!("builtin-other");
        });
        assert!(err.contains("catalog_version"), "{err}");
        // An empty model.
        let err = mutate(&|doc| {
            doc["rows"][0]["model"] = serde_json::json!("  ");
        });
        assert!(err.contains("model"), "{err}");
        // File-level source omitted.
        let err = mutate(&|doc| {
            doc.as_object_mut().unwrap().remove("source");
        });
        assert!(err.contains("source"), "{err}");
        // The untouched vendored bytes DO load and validate.
        load_catalog(BUILTIN_CATALOG_JSON).expect("the checked-in catalog is valid");
    }

    /// An unknown model gets NO price: no lookup entry exists (so callers
    /// stay `PricingState::Unknown` with the ConservativeUnknown quality
    /// placeholder — never a fabricated free/zero row), while retired rows
    /// stay documented-but-excluded and explicit.
    #[test]
    fn unknown_models_get_no_row_and_retired_rows_are_explicit() {
        // No row can ever match an unlisted model under any official
        // origin, through either the origin-keyed or the family shim path.
        for model in [
            "",
            "default",
            "gpt-6",
            "gpt-5-turbo",
            "o9",
            "claude-opus-9",
            "claude-sonnet-9",
            "gemini-9.0-flash",
            "deepseek-v9",
        ] {
            for origin in [
                BillingOrigin::OfficialOpenAi,
                BillingOrigin::OfficialAnthropic,
                BillingOrigin::OfficialGoogle,
                BillingOrigin::OfficialDeepSeek,
            ] {
                assert_eq!(
                    lookup_by_origin(origin, model),
                    None,
                    "{model:?} must have no documented row for {origin:?}"
                );
            }
            for family in ["openai", "anthropic", "google", "deepseek"] {
                assert_eq!(lookup(family, model), None, "{family}/{model:?}");
            }
        }
        // The table has no wildcard/inheritance row: every row names a
        // concrete model token.
        for row in table() {
            assert!(!row.model.is_empty());
            assert!(!row.model.contains('*'));
            assert_eq!(row.model.trim(), row.model);
        }
        // Retired rows are EXPLICIT in the file: exactly the two documented
        // retirements, each carrying its `retired_at_ms`, each excluded
        // from the exact lookup while remaining inspectable.
        let retired: Vec<(&str, u64)> = table()
            .iter()
            .filter(|row| row.is_retired())
            .map(|row| (row.model, row.retired_at_ms.unwrap()))
            .collect();
        assert_eq!(
            retired,
            vec![
                ("claude-sonnet-4", 1_780_272_000_000),
                ("claude-haiku-3.5", 1_769_904_000_000),
            ],
            "retired rows are explicit and frozen"
        );
        for (model, retired_at) in retired {
            let row = lookup_documented_by_origin(BillingOrigin::OfficialAnthropic, model)
                .expect("retired row is documented");
            assert_eq!(row.retired_at_ms, Some(retired_at));
            assert_eq!(
                lookup_by_origin(BillingOrigin::OfficialAnthropic, model),
                None
            );
            assert_eq!(
                lookup_by_origin(
                    BillingOrigin::OfficialAnthropic,
                    &model.to_ascii_uppercase()
                ),
                None
            );
            assert_eq!(
                pricing_state_at(&row, observed_at_ms()),
                PricingState::Unknown,
                "{model} retired must never resolve a price"
            );
            assert_eq!(row.valid_until_ms, Some(retired_at));
        }
        // Every ACTIVE row lacks a retirement date (explicitness is
        // bidirectional: no silent retirement by window expiry).
        for row in table().iter().filter(|row| !row.is_retired()) {
            assert_eq!(row.retired_at_ms, None);
            assert!(
                row.valid_until_ms
                    .is_some_and(|until| until > observed_at_ms()),
                "{} active row window must be open at observation",
                row.model
            );
        }
    }

    #[test]
    fn builtin_rows_are_locked_with_quote_provenance_and_never_free() {
        // Every documented row: exact input/output lines, cache lines only
        // where published, cache_write never silently zero on a
        // write-billing provider, sub-$1/M prices kept exact (no
        // truncation to zero), metadata dated, provenance frozen.
        let mut seen: Vec<(BillingOrigin, &str)> = Vec::new();
        for row in table() {
            assert!(!seen.contains(&(row.origin, row.model)), "duplicate row");
            seen.push((row.origin, row.model));
            let q = quote_of(row);
            assert_eq!(
                q.input.0, row.input_micro_usd_per_million,
                "{} input line exact",
                row.model
            );
            assert_eq!(
                q.output.0, row.output_micro_usd_per_million,
                "{} output line exact",
                row.model
            );
            assert_eq!(
                q.cache_read.0,
                row.cache_read_micro_usd_per_million.unwrap_or(0),
                "{} cache_read line exact",
                row.model
            );
            assert!(!q.is_zero(), "{} must never read as free", row.model);
            assert_ne!(q.input.0, 0, "{} input price nonzero", row.model);
            assert_ne!(q.output.0, 0, "{} output price nonzero", row.model);
            // Every row carries its metadata dates + source identity.
            assert_eq!(row.source_id, BUILTIN_SOURCE_ID);
            assert_eq!(row.catalog_version, BUILTIN_CATALOG_VERSION);
            assert_eq!(row.catalog_version, "builtin-v3");
            assert!(row.effective_at_ms > 0);
            assert!(row.observed_at_ms >= row.effective_at_ms);
            assert!(row.valid_until_ms.is_some());
            assert!(!row.source.trim().is_empty());
            let p = provenance_of(row);
            assert_eq!(p.effective_at_ms, Some(row.effective_at_ms));
            assert_eq!(p.observed_at_ms, Some(row.observed_at_ms));
            assert_eq!(p.valid_until_ms, row.valid_until_ms);
            assert_eq!(p.source_id, row.source_id);
            // Retired rows are documented but never returned as Exact.
            if row.is_retired() {
                assert_eq!(lookup_by_origin(row.origin, row.model), None);
                assert_eq!(
                    lookup_by_origin(row.origin, &row.model.to_ascii_uppercase()),
                    None
                );
                continue;
            }
            assert_eq!(
                lookup_documented_by_origin(row.origin, row.model),
                Some(*row),
                "documented lookup finds every row"
            );
            match row.class {
                BuiltinPriceClass::Exact => {
                    assert_eq!(
                        lookup_by_origin(row.origin, row.model),
                        Some(*row),
                        "active exact row is offered by origin"
                    );
                    assert_eq!(
                        lookup_by_origin(row.origin, &row.model.to_ascii_uppercase()),
                        Some(*row),
                        "origin lookup is case-insensitive on the model"
                    );
                    // The legacy family shim agrees for canonical family
                    // names (exact rows only).
                    let family = family_of(row.origin);
                    assert_eq!(lookup(family, row.model), Some(*row));
                }
                BuiltinPriceClass::ConservativeSchedule => {
                    // Conservative rows are offered by origin (the wrapper
                    // maps them to a ceiling) but NEVER through the raw
                    // exact-only family shim.
                    assert_eq!(
                        lookup_by_origin(row.origin, row.model),
                        Some(*row),
                        "active conservative row is offered by origin"
                    );
                    assert_eq!(lookup(family_of(row.origin), row.model), None);
                }
            }
        }
        // Every documented Anthropic row bills cache writes (never
        // zero-by-omission) and documents its cache reads.
        for row in table()
            .iter()
            .filter(|row| row.origin == BillingOrigin::OfficialAnthropic)
        {
            assert!(
                matches!(row.cache_write, BuiltinCacheWrite::TtlDependent { .. }),
                "{} must carry the TTL-dependent write tariff",
                row.model
            );
            assert!(
                row.cache_read_micro_usd_per_million.is_some_and(|r| r > 0),
                "{} must document its cache-read line",
                row.model
            );
        }
        // The audited goldens settle exactly from the stored rows.
        let gpt4o = quote_of(table().iter().find(|r| r.model == "gpt-4o").unwrap());
        assert_eq!(gpt4o.input.0, 2_500_000, "$2.50/M input, exact");
        assert_eq!(gpt4o.cache_read.0, 1_250_000, "$1.25/M cache read");
        let mini = quote_of(table().iter().find(|r| r.model == "gpt-4o-mini").unwrap());
        assert_eq!(mini.input.0, 150_000, "$0.15/M must NOT truncate to zero");
        assert_eq!(mini.output.0, 600_000, "$0.60/M must NOT truncate to zero");
        // o3's CURRENT repricing is locked (was $10/$40, then $2/$8 with a
        // $0.50/M cached-input line).
        let o3 = quote_of(table().iter().find(|r| r.model == "o3").unwrap());
        assert_eq!(o3.input.0, 2_000_000);
        assert_eq!(o3.output.0, 8_000_000);
        assert_eq!(o3.cache_read.0, 500_000);
        // o4-mini has its separate published cached rate, never zero.
        let o4 = quote_of(table().iter().find(|r| r.model == "o4-mini").unwrap());
        assert_eq!(o4.cache_read.0, 275_000);
        // The refreshed GPT-5 family is priced exactly, sub-cent lines
        // included (nano input $0.05/M, cached $0.005/M).
        let gpt5 = quote_of(table().iter().find(|r| r.model == "gpt-5").unwrap());
        assert_eq!(gpt5.input.0, 1_250_000);
        assert_eq!(gpt5.output.0, 10_000_000);
        assert_eq!(gpt5.cache_read.0, 125_000);
        let nano = quote_of(table().iter().find(|r| r.model == "gpt-5-nano").unwrap());
        assert_eq!(nano.input.0, 50_000, "$0.05/M must NOT truncate to zero");
        assert_eq!(
            nano.cache_read.0, 5_000,
            "$0.005/M must NOT truncate to zero"
        );
    }

    fn family_of(origin: BillingOrigin) -> &'static str {
        match origin {
            BillingOrigin::OfficialOpenAi => "openai",
            BillingOrigin::OfficialAnthropic => "anthropic",
            BillingOrigin::OfficialGoogle => "google",
            BillingOrigin::OfficialDeepSeek => "deepseek",
            _ => panic!("builtin rows are official"),
        }
    }

    #[test]
    fn o3_and_o4_mini_cached_input_is_not_free() {
        // A cached input token is a priced category: 1M cached o3 input
        // tokens cost 500_000 microUSD (never 0), and o4-mini's published
        // cached rate bills 275_000. Zero cache pricing (the builtin-v1
        // lie) would understate both.
        let mut o3 = PricingSnapshot::exact(
            quote_of(table().iter().find(|r| r.model == "o3").unwrap()),
            2,
            BUILTIN_SOURCE_ID.into(),
        );
        o3.valid_until_ms = Some(valid_until_ms());
        assert_eq!(o3.settle_cost(0, 1_000_000, 0, 0), Some(500_000));
        let o4 = quote_of(table().iter().find(|r| r.model == "o4-mini").unwrap());
        assert_eq!(o4.cache_read.0, 275_000);
        assert_eq!(
            PricingSnapshot::exact(o4, 2, BUILTIN_SOURCE_ID.into()).settle_cost(0, 1_000_000, 0, 0),
            Some(275_000)
        );
    }

    #[test]
    fn anthropic_cache_write_is_priced_ttl_dependent_and_max_under_hard_budget() {
        // Anthropic bills cache CREATION. claude-opus-4: 5m write 1.25x
        // input ($18.75/M), 1h write 2x input ($30/M); read 0.1x.
        let row = *table()
            .iter()
            .find(|r| r.model == "claude-opus-4")
            .expect("documented row");
        let sched = BuiltinRowSchedule::new(row, 1);
        let ctx = PricingContext::at(observed_at_ms());
        // A plain quote with an unknown TTL cannot pick a tariff.
        assert_eq!(
            sched.quote(&ctx).unwrap_err(),
            PriceUnavailable::NoApplicableTariff
        );
        // A KNOWN TTL uses exactly its tariff.
        assert_eq!(
            sched
                .quote(&ctx.with_cache_write_ttl(CacheWriteTtl::FiveMinutes))
                .unwrap()
                .quote
                .unwrap()
                .cache_write,
            MicroUsdPerMillionTokens(18_750_000)
        );
        assert_eq!(
            sched
                .quote(&ctx.with_cache_write_ttl(CacheWriteTtl::OneHour))
                .unwrap()
                .quote
                .unwrap()
                .cache_write,
            MicroUsdPerMillionTokens(30_000_000)
        );
        // Unknown tariff + hard budget: the MAXIMUM documented tariff.
        let hard = sched.quote_for_hard_budget(&ctx).unwrap();
        assert_eq!(
            hard.quote.unwrap().cache_write,
            MicroUsdPerMillionTokens(30_000_000),
            "cache creation must never reserve zero or the cheaper TTL"
        );
        // The catalog state reserves the same conservative write tariff.
        let state = pricing_state_at(&row, observed_at_ms());
        assert_eq!(state.authority(), PriceAuthority::Exact);
        let snap = state.snapshot();
        assert_eq!(
            snap.quote.unwrap().cache_write,
            MicroUsdPerMillionTokens(30_000_000)
        );
        assert_eq!(snap.settle_cost(0, 0, 1_000_000, 0), Some(30_000_000));
        assert!(snap.valid_until_ms.is_some());
        // A non-standard service tier has no documented tariff: refuse.
        assert_eq!(
            sched
                .quote(&ctx.with_service_tier(ServiceTier::Priority))
                .unwrap_err(),
            PriceUnavailable::NoApplicableTariff
        );
    }

    #[test]
    fn retired_rows_and_expired_rows_are_unknown() {
        for model in ["claude-sonnet-4", "claude-haiku-3.5"] {
            let row = *table().iter().find(|r| r.model == model).unwrap();
            assert!(row.is_retired());
            assert_eq!(
                pricing_state_at(&row, observed_at_ms()),
                PricingState::Unknown,
                "{model} retired in 2026 must never resolve exact"
            );
        }
        // An ACTIVE row past its window is Unknown too (no ceiling on
        // exact rows: a stale exact quote is not a bound).
        let row = *table().iter().find(|r| r.model == "gpt-4o").unwrap();
        assert_eq!(
            pricing_state_at(&row, row.valid_until_ms.unwrap()),
            PricingState::Unknown
        );
        assert_eq!(
            pricing_state_at(&row, row.valid_until_ms.unwrap() - 1).authority(),
            PriceAuthority::Exact
        );
    }

    #[test]
    fn deepseek_v4_row_is_a_conservative_bound_not_an_exact_claim() {
        let row = *table().iter().find(|r| r.model == "deepseek-chat").unwrap();
        assert_eq!(row.class, BuiltinPriceClass::ConservativeSchedule);
        assert_eq!(
            pricing_state_at(&row, observed_at_ms()).authority(),
            PriceAuthority::ConservativeCeiling,
            "an undocumented V4-era schedule never claims Exact"
        );
        // The bound is a bound: every line is at/above the last published
        // V3.2 rates (270k/1.1M with a 70k cache hit), never below.
        let q = quote_of(&row);
        assert!(q.input.0 >= 270_000);
        assert!(q.output.0 >= 1_100_000);
        assert!(q.cache_read.0 >= 70_000);
        assert!(q.cache_write.0 >= q.input.0, "write bounded at input");
        // The raw exact-only family shim never offers it.
        assert_eq!(lookup("deepseek", "deepseek-chat"), None);
        assert_eq!(
            lookup_by_origin(BillingOrigin::OfficialDeepSeek, "deepseek-chat").map(|r| r.class),
            Some(BuiltinPriceClass::ConservativeSchedule)
        );
    }

    #[test]
    fn lookup_never_invents_rows_and_origin_scoping_is_exact() {
        for (family, model) in [
            ("openai", "gpt-6"),
            ("openai", "gpt-4o-2024-08-06"), // dated snapshots are not rows
            ("ollama", "gpt-4o"),
            ("openai", "my-private-model"),
            ("openai", "claude-sonnet-4"), // origin-scoped: no leak
            ("", ""),
            ("deepseek", "deepseek-reasoner"),
            ("deepseek", "deepseek-chat"), // conservative class: not exact
            ("corp-proxy", "gpt-4o"),      // custom endpoint: never official
        ] {
            assert_eq!(
                lookup(family, model),
                None,
                "{family}/{model} must stay Unknown"
            );
        }
        // The origin-keyed path is scoped too: a custom endpoint or a
        // different official origin never sees another origin's row.
        assert_eq!(
            lookup_by_origin(BillingOrigin::OfficialDeepSeek, "gpt-4o"),
            None
        );
        assert_eq!(
            lookup_by_origin(BillingOrigin::CustomEndpoint, "gpt-4o"),
            None
        );
        assert_eq!(
            lookup_by_origin(BillingOrigin::Gateway, "deepseek-chat"),
            None
        );
        // Retired Anthropic IDs are never Exact lookups.
        for retired in ["claude-sonnet-4", "claude-haiku-3.5"] {
            assert_eq!(
                lookup_by_origin(BillingOrigin::OfficialAnthropic, retired),
                None,
                "{retired} is retired"
            );
        }
        let row = lookup_by_origin(BillingOrigin::OfficialAnthropic, "claude-opus-4")
            .expect("opus-4 is active");
        assert_eq!(row.cache_write.max_rate(), 30_000_000);
    }

    #[test]
    fn performance_priors_are_built_in_priors_and_never_vendor_truths() {
        assert_eq!(PERFORMANCE_TABLE.len(), TABLE.len());
        for row in PERFORMANCE_TABLE {
            let profile = performance_prior(row.origin, row.model).expect("documented prior");
            assert_eq!(
                profile.authority,
                QualityAuthority::BuiltInPrior,
                "{} prior authority",
                row.model
            );
            assert_eq!(profile.benchmark_version, PERFORMANCE_PRIOR_VERSION);
            assert!(profile.prior.context_reliability <= 100);
            assert!(profile.prior.coding_reliability <= 100);
            assert!(profile.prior.estimated_latency_ms > 0);
        }
        // A fresh Balanced configuration must find at least one documented
        // candidate at/above the 88 quality floor for every official origin
        // whose table row exists.
        for origin in [
            BillingOrigin::OfficialOpenAi,
            BillingOrigin::OfficialAnthropic,
            BillingOrigin::OfficialGoogle,
            BillingOrigin::OfficialDeepSeek,
        ] {
            let best = PERFORMANCE_TABLE
                .iter()
                .filter(|r| r.origin == origin)
                .map(|r| r.coding_reliability.max(r.context_reliability))
                .max();
            assert!(
                best.is_some_and(|q| q >= 88),
                "{origin:?} must have a documented prior above the balanced floor"
            );
        }
        // Custom/gateway/local never inherit an official prior.
        assert_eq!(
            performance_prior(BillingOrigin::CustomEndpoint, "gpt-4o"),
            None
        );
        assert_eq!(performance_prior(BillingOrigin::Gateway, "gpt-4o"), None);
    }

    #[test]
    fn provenance_roundtrips_on_the_wire_with_dated_rows() {
        let p = provenance();
        let back: PricingProvenance =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.catalog_version, "builtin-v3");
        assert_eq!(back.source_id, "faktor-builtin-v3");
        assert_eq!(back.effective_at_ms, Some(effective_at_ms()));
        assert_eq!(back.observed_at_ms, Some(observed_at_ms()));
        assert_eq!(back.valid_until_ms, Some(valid_until_ms()));
        assert!(back.observed_at_ms >= back.effective_at_ms);
        // Legacy provenance JSON without the new/optional fields still
        // decodes (Option defaults).
        let legacy: PricingProvenance = serde_json::from_value(serde_json::json!({
            "source_id": "old", "catalog_version": "builtin-v1"
        }))
        .unwrap();
        assert_eq!(legacy.effective_at_ms, None);
        assert_eq!(legacy.observed_at_ms, None);
        assert_eq!(legacy.valid_until_ms, None);
    }
}
