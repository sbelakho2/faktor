//! Typed operation requests and per-operation validation.
//!
//! The `source_market` tool schema is deliberately flat (`docs/acquire.md`
//! §5); Rust performs the real validation here, once, with typed bounds:
//!
//! | bound | value |
//! |-------|-------|
//! | query length | [`MAX_QUERY_BYTES`] = 512 |
//! | product ref length | [`MAX_REF_BYTES`] = 2048 |
//! | result limit | [`MAX_LIMIT`] = 50 |
//! | quantity | 1 ..= [`crate::quantity::MAX_ORDER_QUANTITY`] |
//! | BOM lines | [`MAX_BOM_LINES`] = 500 |
//! | sources per request | [`MAX_SOURCES`] = 6 |
//!
//! Every rejection is a typed [`RequestError`] that converts to
//! [`SourceError::InvalidRequest`]; nothing is silently clamped.

use serde::{Deserialize, Serialize};

use crate::error::SourceError;
use crate::identity::PartNumberNormalizer;
use crate::packaging::PackagingType;
use crate::quantity::{NonZeroQuantity, MAX_ORDER_QUANTITY};
use crate::text::{AccountScope, CanonicalUrl, SourceId, Text, VariantId};

/// Byte bound of a free-text query.
pub const MAX_QUERY_BYTES: usize = 512;
/// Byte bound of a product reference (`ref`).
pub const MAX_REF_BYTES: usize = 2048;
/// Maximum result limit.
pub const MAX_LIMIT: u8 = 50;
/// Minimum result limit.
pub const MIN_LIMIT: u8 = 1;
/// Maximum BOM lines.
pub const MAX_BOM_LINES: usize = 500;
/// Maximum sources per request.
pub const MAX_SOURCES: usize = 6;
/// Byte bound of a source SKU.
pub const MAX_SKU_BYTES: usize = 128;
/// Byte bound of a source offer id.
pub const MAX_OFFER_ID_BYTES: usize = 128;
/// Byte bound of an MPN query.
pub const MAX_PART_NUMBER_BYTES: usize = 256;

/// The tool operation (`docs/acquire.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Discovery only.
    Search,
    /// Inspect one known reference.
    Product,
    /// A real applicable price at a quantity.
    Quote,
    /// Many items.
    Bom,
    /// Deterministic state/result of a bulk job.
    Job,
}

impl Op {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Product => "product",
            Self::Quote => "quote",
            Self::Bom => "bom",
            Self::Job => "job",
        }
    }

    /// Parse the wire label.
    pub fn parse(raw: &str) -> Result<Self, RequestError> {
        match raw {
            "search" => Ok(Self::Search),
            "product" => Ok(Self::Product),
            "quote" => Ok(Self::Quote),
            "bom" => Ok(Self::Bom),
            "job" => Ok(Self::Job),
            other => Err(RequestError::new(
                "op",
                format!("unknown operation {other:?}"),
            )),
        }
    }
}

/// Requested freshness (`docs/acquire.md` §5, §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessMode {
    /// Fresh cache first, then live; a stale cache entry may be returned as
    /// an explicit stale fallback when the live path fails.
    #[default]
    PreferCache,
    /// Always acquire live; never present a cache entry as current.
    Live,
    /// Never touch the network; fresh cache, else stale fallback, else a
    /// typed cache miss.
    CacheOnly,
}

impl FreshnessMode {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreferCache => "prefer_cache",
            Self::Live => "live",
            Self::CacheOnly => "cache_only",
        }
    }

    /// Parse the wire label.
    pub fn parse(raw: &str) -> Result<Self, RequestError> {
        match raw {
            "prefer_cache" => Ok(Self::PreferCache),
            "live" => Ok(Self::Live),
            "cache_only" => Ok(Self::CacheOnly),
            other => Err(RequestError::new(
                "freshness",
                format!("unknown freshness {other:?}"),
            )),
        }
    }
}

/// Requested detail level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    /// Compact: counts, important entries, artifact reference.
    #[default]
    Compact,
    /// Normal: the normalized result with provenance.
    Normal,
    /// Full: the normalized result plus bounded diagnostics.
    Full,
}

impl DetailLevel {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Full => "full",
        }
    }

    /// Parse the wire label.
    pub fn parse(raw: &str) -> Result<Self, RequestError> {
        match raw {
            "compact" => Ok(Self::Compact),
            "normal" => Ok(Self::Normal),
            "full" => Ok(Self::Full),
            other => Err(RequestError::new(
                "detail",
                format!("unknown detail level {other:?}"),
            )),
        }
    }
}

/// A rejected request. Every variant is typed; nothing is clamped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {field}: {reason}")]
pub struct RequestError {
    /// The offending field.
    pub field: &'static str,
    /// Why it was rejected.
    pub reason: String,
}

impl RequestError {
    /// Construct one rejection.
    pub fn new(field: &'static str, reason: impl Into<String>) -> Self {
        Self {
            field,
            reason: reason.into(),
        }
    }
}

impl From<RequestError> for SourceError {
    fn from(error: RequestError) -> Self {
        tracing::debug!("commerce request rejected: {error}");
        SourceError::InvalidRequest
    }
}

/// The requested source set: `auto` (all capable sources) or an explicit,
/// bounded, duplicate-free list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceSet(Vec<SourceId>);

impl SourceSet {
    /// Automatic source selection.
    pub fn auto() -> Self {
        Self(Vec::new())
    }

    /// An explicit source list: 1..=6 entries, duplicate-free; the literal
    /// `auto` is never a source id.
    pub fn named(sources: Vec<SourceId>) -> Result<Self, RequestError> {
        if sources.is_empty() {
            return Err(RequestError::new(
                "sources",
                "an explicit source list is empty",
            ));
        }
        if sources.len() > MAX_SOURCES {
            return Err(RequestError::new(
                "sources",
                format!(
                    "{} sources exceeds the maximum {MAX_SOURCES}",
                    sources.len()
                ),
            ));
        }
        let mut seen: Vec<SourceId> = Vec::new();
        for source in &sources {
            if source.as_str() == "auto" {
                return Err(RequestError::new(
                    "sources",
                    "\"auto\" cannot be mixed with explicit sources",
                ));
            }
            if seen.contains(source) {
                return Err(RequestError::new(
                    "sources",
                    format!("duplicate source {source}"),
                ));
            }
            seen.push(source.clone());
        }
        Ok(Self(sources))
    }

    /// True for automatic selection.
    pub fn is_auto(&self) -> bool {
        self.0.is_empty()
    }

    /// The explicit sources (empty for `auto`).
    pub fn as_slice(&self) -> &[SourceId] {
        &self.0
    }

    /// Validate a deserialized value (used when loading durable requests).
    pub fn validate(&self) -> Result<(), RequestError> {
        if self.0.is_empty() {
            return Ok(());
        }
        Self::named(self.0.clone()).map(|_| ())
    }
}

/// A reference to one product: a canonical URL, a source SKU, a source offer
/// id, or a bare part number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProductRef {
    /// A canonical product URL.
    Url {
        /// The validated URL.
        url: CanonicalUrl,
    },
    /// A source-scoped SKU.
    SourceSku {
        /// The source.
        source: SourceId,
        /// The SKU.
        sku: Text<{ MAX_SKU_BYTES }>,
    },
    /// A source-scoped offer id.
    OfferId {
        /// The source.
        source: SourceId,
        /// The offer id.
        offer_id: Text<{ MAX_OFFER_ID_BYTES }>,
    },
    /// A bare part number (MPN or source part number).
    PartNumber {
        /// The part number.
        part_number: Text<{ MAX_PART_NUMBER_BYTES }>,
    },
}

impl ProductRef {
    /// Parse the tool's `ref` string deterministically: a `http(s)://` URL is
    /// a [`ProductRef::Url`]; otherwise a source-scoped SKU when a source
    /// hint is present, else a part number.
    pub fn parse(raw: &str, source: Option<&SourceId>) -> Result<Self, RequestError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(RequestError::new("ref", "reference is empty"));
        }
        if trimmed.len() > MAX_REF_BYTES {
            return Err(RequestError::new(
                "ref",
                format!("reference exceeds {MAX_REF_BYTES} bytes"),
            ));
        }
        let lowered = trimmed.to_ascii_lowercase();
        if lowered.starts_with("http://") || lowered.starts_with("https://") {
            let url = CanonicalUrl::parse(trimmed)
                .map_err(|error| RequestError::new("ref", format!("invalid url: {error}")))?;
            return Ok(Self::Url { url });
        }
        match source {
            Some(source) => {
                let sku = Text::<MAX_SKU_BYTES>::new(trimmed)
                    .map_err(|error| RequestError::new("ref", format!("invalid sku: {error}")))?;
                Ok(Self::SourceSku {
                    source: source.clone(),
                    sku,
                })
            }
            None => {
                let part_number = Text::<MAX_PART_NUMBER_BYTES>::new(trimmed).map_err(|error| {
                    RequestError::new("ref", format!("invalid part number: {error}"))
                })?;
                PartNumberNormalizer::new()
                    .normalize(part_number.as_str())
                    .map_err(|error| {
                        RequestError::new("ref", format!("unusable part number: {error}"))
                    })?;
                Ok(Self::PartNumber { part_number })
            }
        }
    }

    /// The source this reference is scoped to, when one is known.
    pub fn source(&self) -> Option<&SourceId> {
        match self {
            Self::Url { .. } | Self::PartNumber { .. } => None,
            Self::SourceSku { source, .. } | Self::OfferId { source, .. } => Some(source),
        }
    }

    /// The part-number text this reference carries, when it has one.
    pub fn part_number_text(&self) -> Option<&str> {
        match self {
            Self::PartNumber { part_number } => Some(part_number.as_str()),
            _ => None,
        }
    }

    /// A stable identity string for cache/digest purposes. A bare part
    /// number uses the canonical (NON-lossy) [`PartNumberNormalizer`] form:
    /// punctuation that is part of one part number (`AB#123`) must never be
    /// erased into another (`AB123`), while normalization-equivalent forms
    /// (case, width, whitespace) still share one identity. Only a value that
    /// cannot be normalized at all falls back to a raw (still non-lossy)
    /// spelling.
    pub fn identity_key(&self) -> String {
        match self {
            Self::Url { url } => format!("url:{}", url.as_str()),
            Self::SourceSku { source, sku } => format!("sku:{source}:{}", sku.as_str()),
            Self::OfferId { source, offer_id } => {
                format!("offer:{source}:{}", offer_id.as_str())
            }
            Self::PartNumber { part_number } => {
                match PartNumberNormalizer::new().normalize(part_number.as_str()) {
                    Ok(normalized) => format!("mpn:{}", normalized.canonical()),
                    Err(_) => format!(
                        "mpn-raw:{}",
                        part_number.as_str().trim().to_ascii_uppercase()
                    ),
                }
            }
        }
    }

    /// Validate a deserialized value.
    pub fn validate(&self) -> Result<(), RequestError> {
        match self {
            Self::PartNumber { part_number } => PartNumberNormalizer::new()
                .normalize(part_number.as_str())
                .map(|_| ())
                .map_err(|error| {
                    RequestError::new("ref", format!("unusable part number: {error}"))
                }),
            _ => Ok(()),
        }
    }
}

/// Deterministic query normalization used for identity/digest purposes only
/// (never for display): lowercase ASCII, collapse whitespace, drop
/// punctuation that is not part of a part number. This is the FREE-TEXT
/// search normalization; part-number and BOM-line identities use the
/// non-lossy [`canonical_query`] instead.
pub fn normalize_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        let lowered = ch.to_ascii_lowercase();
        if !(lowered.is_ascii_alphanumeric()
            || matches!(lowered, '+' | '-' | '.' | '_' | '/' | ':'))
        {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(lowered);
    }
    out
}

/// The NON-LOSSY canonical form of one query used for line/job identity:
/// ASCII letters lowercased, runs of whitespace collapsed to one space, and
/// every other character preserved. Identity must never erase punctuation
/// that distinguishes two queries — `AB#123` and `AB123` are different
/// lines — while case/whitespace-equivalent spellings still share one
/// identity.
pub fn canonical_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Discovery request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    /// The free-text query (≤ 512 bytes).
    pub query: Text<{ MAX_QUERY_BYTES }>,
    /// The requested sources.
    pub sources: SourceSet,
    /// The result limit (1..=50).
    pub limit: u8,
    /// The requested freshness.
    pub freshness: FreshnessMode,
    /// The requested detail level.
    pub detail: DetailLevel,
}

impl SearchRequest {
    /// Validate and construct.
    pub fn new(
        query: &str,
        sources: SourceSet,
        limit: u8,
        freshness: FreshnessMode,
        detail: DetailLevel,
    ) -> Result<Self, RequestError> {
        let query = Text::<MAX_QUERY_BYTES>::new(query)
            .map_err(|error| RequestError::new("q", error.to_string()))?;
        let request = Self {
            query,
            sources,
            limit,
            freshness,
            detail,
        };
        request.validate()?;
        Ok(request)
    }

    /// Re-validate every bound (also used when loading durable requests).
    pub fn validate(&self) -> Result<(), RequestError> {
        if !(MIN_LIMIT..=MAX_LIMIT).contains(&self.limit) {
            return Err(RequestError::new(
                "limit",
                format!("limit {} is outside {MIN_LIMIT}..={MAX_LIMIT}", self.limit),
            ));
        }
        self.sources.validate()?;
        if self.query.len() > MAX_QUERY_BYTES {
            return Err(RequestError::new("q", "query exceeds 512 bytes"));
        }
        Ok(())
    }

    /// The normalized acquisition identity of this query.
    pub fn identity_digest(&self) -> String {
        normalize_query(self.query.as_str())
    }
}

/// Product inspection request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRequest {
    /// The reference to inspect.
    pub reference: ProductRef,
    /// The requested freshness.
    pub freshness: FreshnessMode,
    /// The requested detail level.
    pub detail: DetailLevel,
}

impl ProductRequest {
    /// Validate and construct.
    pub fn new(
        reference: ProductRef,
        freshness: FreshnessMode,
        detail: DetailLevel,
    ) -> Result<Self, RequestError> {
        reference.validate()?;
        Ok(Self {
            reference,
            freshness,
            detail,
        })
    }

    /// Re-validate (also used when loading durable requests).
    pub fn validate(&self) -> Result<(), RequestError> {
        self.reference.validate()
    }
}

/// Quote request: a real applicable price at a quantity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteRequest {
    /// The reference to quote.
    pub reference: ProductRef,
    /// The requested quantity (1..=1e9).
    pub quantity: NonZeroQuantity,
    /// The requested packaging, when the caller cares.
    pub packaging: Option<PackagingType>,
    /// The requested variant, when the caller knows it.
    pub variant: Option<VariantId>,
    /// The account scope (account pricing is never applied without it).
    pub account: Option<AccountScope>,
    /// The requested freshness.
    pub freshness: FreshnessMode,
}

impl QuoteRequest {
    /// Validate and construct.
    pub fn new(
        reference: ProductRef,
        quantity: u64,
        packaging: Option<PackagingType>,
        variant: Option<VariantId>,
        account: Option<AccountScope>,
        freshness: FreshnessMode,
    ) -> Result<Self, RequestError> {
        let quantity = NonZeroQuantity::new(quantity).map_err(|error| {
            RequestError::new("qty", format!("{error} (maximum {MAX_ORDER_QUANTITY})"))
        })?;
        reference.validate()?;
        Ok(Self {
            reference,
            quantity,
            packaging,
            variant,
            account,
            freshness,
        })
    }

    /// Re-validate (also used when loading durable requests).
    pub fn validate(&self) -> Result<(), RequestError> {
        self.reference.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> SourceId {
        SourceId::new(id).expect("source")
    }

    #[test]
    fn query_bounds_are_enforced_typed() {
        assert!(SearchRequest::new(
            &"x".repeat(MAX_QUERY_BYTES),
            SourceSet::auto(),
            50,
            FreshnessMode::Live,
            DetailLevel::Compact
        )
        .is_ok());
        assert!(SearchRequest::new(
            &"x".repeat(MAX_QUERY_BYTES + 1),
            SourceSet::auto(),
            50,
            FreshnessMode::Live,
            DetailLevel::Compact
        )
        .is_err());
        assert!(SearchRequest::new(
            "ok",
            SourceSet::auto(),
            0,
            FreshnessMode::Live,
            DetailLevel::Compact
        )
        .is_err());
        assert!(SearchRequest::new(
            "ok",
            SourceSet::auto(),
            51,
            FreshnessMode::Live,
            DetailLevel::Compact
        )
        .is_err());
        assert!(SearchRequest::new(
            "   ",
            SourceSet::auto(),
            10,
            FreshnessMode::Live,
            DetailLevel::Compact
        )
        .is_err());
    }

    #[test]
    fn source_sets_are_bounded_and_never_accept_auto_as_a_source() {
        assert!(SourceSet::auto().is_auto());
        assert!(SourceSet::named(vec![source("mouser"), source("lcsc")]).is_ok());
        assert!(SourceSet::named(Vec::new()).is_err());
        assert!(SourceSet::named(vec![source("auto")]).is_err());
        assert!(SourceSet::named(vec![source("mouser"), source("Mouser")]).is_err());
        let seven: Vec<SourceId> = (0..7).map(|i| source(&format!("s{i}"))).collect();
        assert!(SourceSet::named(seven).is_err());
    }

    #[test]
    fn quantities_are_bounded() {
        let reference = ProductRef::parse("TPS5430DDAR", None).expect("ref");
        assert!(
            QuoteRequest::new(reference.clone(), 0, None, None, None, FreshnessMode::Live).is_err()
        );
        assert!(QuoteRequest::new(
            reference.clone(),
            1_000_000_000,
            None,
            None,
            None,
            FreshnessMode::Live
        )
        .is_ok());
        assert!(QuoteRequest::new(
            reference,
            1_000_000_001,
            None,
            None,
            None,
            FreshnessMode::Live
        )
        .is_err());
    }

    #[test]
    fn ref_parsing_is_deterministic() {
        assert!(matches!(
            ProductRef::parse("https://detail.1688.com/offer/123.html", None),
            Ok(ProductRef::Url { .. })
        ));
        assert!(matches!(
            ProductRef::parse("C12345", Some(&source("lcsc"))),
            Ok(ProductRef::SourceSku { .. })
        ));
        assert!(matches!(
            ProductRef::parse("TPS5430DDAR", None),
            Ok(ProductRef::PartNumber { .. })
        ));
        assert!(ProductRef::parse("", None).is_err());
        assert!(ProductRef::parse("###", None).is_err());
        assert!(ProductRef::parse(&"x".repeat(MAX_REF_BYTES + 1), None).is_err());
    }

    #[test]
    fn query_normalization_is_stable() {
        assert_eq!(normalize_query("  TPS5430DDAR  "), "tps5430ddar");
        assert_eq!(normalize_query("STM32F407  VGT6"), "stm32f407 vgt6");
        assert_eq!(normalize_query("TPS5430-DDAR"), "tps5430-ddar");
        assert_eq!(normalize_query("100nF 0402 (10%)"), "100nf 0402 10");
        assert_eq!(normalize_query("A, B; C"), "a b c");
    }

    #[test]
    fn part_number_identity_is_canonical_and_lossless() {
        let with_hash = ProductRef::parse("AB#123", None).expect("ref");
        let plain = ProductRef::parse("AB123", None).expect("ref");
        assert_ne!(
            with_hash.identity_key(),
            plain.identity_key(),
            "punctuation that is part of one part number must never be erased into another"
        );
        for spelling in [" tps5430ddar ", "TPS5430DDAR", "tps 5430ddar"] {
            assert_eq!(
                ProductRef::parse(spelling, None)
                    .expect("ref")
                    .identity_key(),
                ProductRef::parse("TPS5430DDAR", None)
                    .expect("ref")
                    .identity_key(),
                "{spelling} is normalization-equivalent"
            );
        }
    }
}
