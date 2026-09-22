//! The canonical commercial offer model.
//!
//! An offer is one purchasable listing as observed from one source: its
//! identity, its price tiers, its variants, its packagings, its stock and
//! lead time, its supplier/manufacturer evidence and the provenance of the
//! observation. Every list is bounded, every cross-field invariant is
//! validated on deserialization, and unknown JSON fields are rejected.
//!
//! Invariants enforced by [`CommercialOffer::validate`] (and by
//! deserialization):
//!
//! - one currency per offer: every [`Money`] inside it uses
//!   [`CommercialOffer::currency`];
//! - [`PriceVisibility::InquiryRequired`] (and `Unknown`) means *no price
//!   at all*: such an offer can never carry a fabricated price;
//! - price tiers never overlap and never repeat a minimum quantity;
//! - prices are never negative;
//! - a promotion never discounts a tier below zero;
//! - variant ids and attribute names are unique.

use serde::{Deserialize, Serialize};

use crate::identity::ProductIdentity;
use crate::money::{BasisPoints, Currency, Money};
use crate::packaging::PackagingType;
use crate::quantity::{NonZeroQuantity, Quantity};
use crate::text::{AccountScope, CanonicalUrl, SourceId, Text, VariantId};

/// Maximum variants per offer.
pub const MAX_VARIANTS_PER_OFFER: usize = 256;
/// Maximum price breaks per price-break list.
pub const MAX_PRICE_BREAKS_PER_LIST: usize = 64;
/// Maximum packaging options per offer.
pub const MAX_PACKAGING_OPTIONS: usize = 32;
/// Maximum attributes per variant.
pub const MAX_ATTRIBUTES_PER_VARIANT: usize = 32;

/// How visible a price is to the requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceVisibility {
    /// A public price.
    Public,
    /// Visible only when authenticated.
    Authenticated,
    /// Visible only for a specific account (never shared across scopes).
    AccountSpecific,
    /// A promotional price.
    Promotional,
    /// No price exists without an RFQ / contacting the supplier.
    InquiryRequired,
    /// The source did not state visibility.
    Unknown,
}

/// The origin of an observation, in documented authority order (highest
/// first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationOrigin {
    /// An official marketplace API.
    OfficialApi,
    /// A first-party JSON/HTTP response.
    HttpJson,
    /// A captured browser network response (XHR/fetch).
    BrowserNetwork,
    /// Embedded page/application state.
    EmbeddedState,
    /// Structured markup (JSON-LD, microdata).
    StructuredMarkup,
    /// Semantic DOM extraction.
    Dom,
    /// Rendered-text fallback.
    RenderedText,
}

impl ObservationOrigin {
    /// The documented authority rank (higher is stronger). Never averaged.
    pub const fn authority_rank(self) -> u8 {
        match self {
            Self::OfficialApi => 7,
            Self::HttpJson => 6,
            Self::BrowserNetwork => 5,
            Self::EmbeddedState => 4,
            Self::StructuredMarkup => 3,
            Self::Dom => 2,
            Self::RenderedText => 1,
        }
    }

    /// A stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfficialApi => "official_api",
            Self::HttpJson => "http_json",
            Self::BrowserNetwork => "browser_network",
            Self::EmbeddedState => "embedded_state",
            Self::StructuredMarkup => "structured_markup",
            Self::Dom => "dom",
            Self::RenderedText => "rendered_text",
        }
    }
}

/// A validated confidence in basis points (`0..=10000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct ConfidenceBp(u16);

impl ConfidenceBp {
    /// Validate a confidence value.
    pub fn new(value: u16) -> Result<Self, ConfidenceError> {
        if value > 10_000 {
            return Err(ConfidenceError { actual: value });
        }
        Ok(Self(value))
    }

    /// The raw value.
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ConfidenceBp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = u16::deserialize(deserializer)?;
        ConfidenceBp::new(raw).map_err(serde::de::Error::custom)
    }
}

/// A rejected confidence value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("confidence {actual} basis points exceeds 10000")]
pub struct ConfidenceError {
    /// The rejected value.
    pub actual: u16,
}

/// Where an offer came from and how it was obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfferProvenance {
    /// The extraction origin.
    pub origin: ObservationOrigin,
    /// The source that produced the observation.
    pub source: SourceId,
    /// The extractor version, when the runtime knows it.
    pub extractor_version: Option<Text<64>>,
    /// The connector version, when the runtime knows it.
    pub connector_version: Option<Text<64>>,
    /// The normalization version, when the runtime knows it.
    pub normalization_version: Option<Text<64>>,
    /// A digest of the normalized snapshot this offer was built from.
    pub content_digest: Option<Text<80>>,
    /// The account scope this observation belongs to.
    pub account_scope: Option<AccountScope>,
    /// The locale of the source page.
    pub locale: Option<Text<32>>,
    /// The market of the source page.
    pub market: Option<Text<32>>,
    /// The source's own confidence in the extracted fields, if stated.
    pub source_confidence_bp: Option<ConfidenceBp>,
}

impl OfferProvenance {
    /// Validate digest shape (bare 64-hex or `blake3:<64-hex>`).
    pub fn validate(&self) -> Result<(), OfferError> {
        if let Some(digest) = &self.content_digest {
            let value = digest.as_str();
            let bare = value.strip_prefix("blake3:").unwrap_or(value);
            if bare.len() != 64 || !bare.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(OfferError::InvalidDigest {
                    value: value.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// How fresh an observation is. A stale fallback is never presented as
/// current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Freshness {
    /// Observed live in this request.
    Live,
    /// Served from a fresh cache entry.
    FreshCache {
        /// The age of the cache entry.
        age_ms: u64,
    },
    /// Served from a stale cache entry as a fallback.
    StaleFallback {
        /// The age of the cache entry.
        age_ms: u64,
    },
}

impl Freshness {
    /// True when the value may be presented as current.
    pub const fn is_current(self) -> bool {
        matches!(self, Self::Live | Self::FreshCache { .. })
    }

    /// True when the value is a stale fallback.
    pub const fn is_stale(self) -> bool {
        matches!(self, Self::StaleFallback { .. })
    }

    /// The cache age, when there is one.
    pub const fn age_ms(self) -> Option<u64> {
        match self {
            Self::Live => None,
            Self::FreshCache { age_ms } | Self::StaleFallback { age_ms } => Some(age_ms),
        }
    }
}

/// Stock state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StockState {
    /// In stock with a known quantity.
    InStock {
        /// The available quantity (at least one).
        quantity: NonZeroQuantity,
    },
    /// Known to be out of stock.
    OutOfStock,
    /// On backorder, quantity and lead time optional.
    Backorder {
        /// The backordered quantity, when stated.
        quantity: Option<NonZeroQuantity>,
        /// The expected lead time, when stated.
        lead_time: Option<LeadTime>,
    },
    /// The source did not state stock.
    Unknown,
}

impl StockState {
    /// The known available quantity.
    pub const fn available_quantity(self) -> Option<u64> {
        match self {
            Self::InStock { quantity } => Some(quantity.get()),
            Self::Backorder { quantity, .. } => match quantity {
                Some(quantity) => Some(quantity.get()),
                None => None,
            },
            Self::OutOfStock | Self::Unknown => None,
        }
    }

    /// True for known in-stock.
    pub const fn is_in_stock(self) -> bool {
        matches!(self, Self::InStock { .. })
    }

    /// True when stock is known at all.
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// A lead time in days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LeadTime {
    /// Minimum days.
    pub min_days: u16,
    /// Maximum days.
    pub max_days: u16,
}

impl LeadTime {
    /// Validate `min_days <= max_days`.
    pub fn new(min_days: u16, max_days: u16) -> Result<Self, OfferError> {
        if min_days > max_days {
            return Err(OfferError::InvalidLeadTime { min_days, max_days });
        }
        Ok(Self { min_days, max_days })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLeadTime {
    min_days: u16,
    max_days: u16,
}

impl TryFrom<RawLeadTime> for LeadTime {
    type Error = OfferError;

    fn try_from(raw: RawLeadTime) -> Result<Self, Self::Error> {
        LeadTime::new(raw.min_days, raw.max_days)
    }
}

impl<'de> Deserialize<'de> for LeadTime {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawLeadTime::deserialize(deserializer)?;
        LeadTime::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// A supplier as reported by a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supplier {
    /// The supplier's identifier at the source.
    pub id: Option<Text<128>>,
    /// The supplier's display name.
    pub name: Text<256>,
    /// The supplier's country, when stated.
    pub country: Option<Text<64>>,
    /// The supplier's canonical URL.
    pub url: Option<CanonicalUrl>,
    /// The account scope this supplier data was observed under.
    pub account_scope: Option<AccountScope>,
}

impl Supplier {
    /// Evidence completeness in basis points (present fields / 4).
    pub fn completeness_bp(&self) -> u16 {
        let mut present = 0u16;
        for flag in [
            self.id.is_some(),
            self.country.is_some(),
            self.url.is_some(),
            self.account_scope.is_some(),
        ] {
            if flag {
                present += 1;
            }
        }
        present * 2_500
    }
}

/// A manufacturer as reported by a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manufacturer {
    /// The manufacturer's identifier.
    pub id: Option<Text<128>>,
    /// The manufacturer's display name.
    pub name: Text<256>,
    /// The manufacturer's country, when stated.
    pub country: Option<Text<64>>,
    /// The manufacturer's canonical URL.
    pub url: Option<CanonicalUrl>,
}

impl Manufacturer {
    /// Evidence completeness in basis points (present fields / 4).
    pub fn completeness_bp(&self) -> u16 {
        let mut present = 0u16;
        for flag in [
            self.id.is_some(),
            self.country.is_some(),
            self.url.is_some(),
            !self.name.as_str().is_empty(),
        ] {
            if flag {
                present += 1;
            }
        }
        present * 2_500
    }
}

/// One attribute of a variant (for example `resistance = 10k`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantAttribute {
    /// The attribute name.
    pub name: Text<64>,
    /// The attribute value.
    pub value: Text<256>,
}

impl VariantAttribute {
    /// Case- and space-insensitive attribute equality.
    pub fn matches(&self, other: &Self) -> bool {
        normalize_attribute(self.name.as_str()) == normalize_attribute(other.name.as_str())
            && normalize_attribute(self.value.as_str()) == normalize_attribute(other.value.as_str())
    }
}

fn normalize_attribute(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// An explicit promotion attached to a price tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Promotion {
    /// The promotion identifier (stable, source-provided).
    pub id: Text<64>,
    /// The promotion kind.
    pub kind: PromotionKind,
    /// When the promotion expires, if stated.
    pub valid_until_ms: Option<u64>,
    /// A human label.
    pub label: Option<Text<128>>,
}

impl Promotion {
    /// True when the promotion is active at `now_ms`.
    pub fn is_active_at(&self, now_ms: u64) -> bool {
        match self.valid_until_ms {
            Some(until) => now_ms < until,
            None => true,
        }
    }
}

/// The kind of an explicit promotion. All arithmetic is integer-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PromotionKind {
    /// A percentage discount per unit.
    PercentOff {
        /// The discount in basis points.
        basis_points: BasisPoints,
    },
    /// A fixed amount off per unit.
    AmountOff {
        /// The amount removed from the unit price.
        per_unit: Money,
    },
    /// Buy `pay_units`, get `free_units` free.
    FreeUnits {
        /// The paid units per bundle.
        pay_units: NonZeroQuantity,
        /// The free units per bundle.
        free_units: NonZeroQuantity,
    },
    /// Buy `quantity` units for a total `price`.
    BundlePrice {
        /// The bundle size.
        quantity: NonZeroQuantity,
        /// The bundle price.
        price: Money,
    },
}

/// One price tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawPriceBreak", into = "RawPriceBreak")]
pub struct PriceBreak {
    /// The inclusive minimum quantity.
    pub min_quantity: NonZeroQuantity,
    /// The inclusive maximum quantity, when the tier is bounded.
    pub max_quantity: Option<NonZeroQuantity>,
    /// The unit price of this tier.
    pub unit_price: Money,
    /// The visibility of this price.
    pub visibility: PriceVisibility,
    /// The account scope this tier applies to.
    pub account_scope: Option<AccountScope>,
    /// An explicit promotion attached to this tier.
    pub promotion: Option<Promotion>,
}

impl PriceBreak {
    /// True when this tier applies at `quantity` for `account`.
    pub fn applies_at(&self, quantity: Quantity, account: Option<&AccountScope>) -> bool {
        if quantity < self.min_quantity.as_quantity() {
            return false;
        }
        if let Some(max) = self.max_quantity {
            if quantity > max.as_quantity() {
                return false;
            }
        }
        match &self.account_scope {
            Some(scope) => account == Some(scope),
            None => true,
        }
    }

    /// True when this tier is account-specific.
    pub fn is_account_specific(&self) -> bool {
        self.account_scope.is_some()
    }

    /// Validate this tier in isolation.
    pub fn validate(&self, expected_currency: Currency) -> Result<(), OfferError> {
        if self.unit_price.currency != expected_currency {
            return Err(OfferError::CurrencyMismatch {
                field: "price_break.unit_price",
                expected: expected_currency,
                actual: self.unit_price.currency,
            });
        }
        if self.unit_price.is_negative() {
            return Err(OfferError::NegativePrice {
                field: "price_break.unit_price",
            });
        }
        if let Some(max) = self.max_quantity {
            if max < self.min_quantity {
                return Err(OfferError::InvalidTierRange {
                    min: self.min_quantity.get(),
                    max: max.get(),
                });
            }
        }
        if self.visibility == PriceVisibility::InquiryRequired {
            return Err(OfferError::PriceWithInquiryVisibility);
        }
        match self.visibility {
            PriceVisibility::AccountSpecific if self.account_scope.is_none() => {
                return Err(OfferError::AccountScopeRequired);
            }
            PriceVisibility::Public | PriceVisibility::Authenticated | PriceVisibility::Unknown
                if self.account_scope.is_some() =>
            {
                return Err(OfferError::AccountScopeUnexpected);
            }
            _ => {}
        }
        if let Some(promotion) = &self.promotion {
            validate_promotion(promotion, self.unit_price)?;
        }
        Ok(())
    }
}

fn validate_promotion(promotion: &Promotion, unit_price: Money) -> Result<(), OfferError> {
    match &promotion.kind {
        PromotionKind::PercentOff { basis_points: _ } => {}
        PromotionKind::AmountOff { per_unit } => {
            if per_unit.currency != unit_price.currency {
                return Err(OfferError::CurrencyMismatch {
                    field: "promotion.per_unit",
                    expected: unit_price.currency,
                    actual: per_unit.currency,
                });
            }
            if per_unit.is_negative() {
                return Err(OfferError::NegativePrice {
                    field: "promotion.per_unit",
                });
            }
            if per_unit.micros > unit_price.micros {
                return Err(OfferError::InvalidPromotion {
                    reason: "amount off exceeds the unit price".to_string(),
                });
            }
        }
        PromotionKind::FreeUnits { .. } => {}
        PromotionKind::BundlePrice { price, .. } => {
            if price.currency != unit_price.currency {
                return Err(OfferError::CurrencyMismatch {
                    field: "promotion.price",
                    expected: unit_price.currency,
                    actual: price.currency,
                });
            }
            if price.is_negative() {
                return Err(OfferError::NegativePrice {
                    field: "promotion.price",
                });
            }
        }
    }
    Ok(())
}

/// Validate a list of tiers: no duplicate minimum, no overlap.
pub fn validate_price_breaks(
    breaks: &[PriceBreak],
    expected_currency: Currency,
) -> Result<(), OfferError> {
    if breaks.len() > MAX_PRICE_BREAKS_PER_LIST {
        return Err(OfferError::TooManyPriceBreaks {
            max: MAX_PRICE_BREAKS_PER_LIST,
            actual: breaks.len(),
        });
    }
    for price_break in breaks {
        price_break.validate(expected_currency)?;
    }
    // A tier without an explicit maximum is implicitly bounded by the next
    // higher minimum, so only consecutive tiers (ordered by minimum) can
    // overlap. A gap between an explicit maximum and the next minimum is
    // allowed and resolves to `NoPrice` inside the gap.
    let mut order: Vec<usize> = (0..breaks.len()).collect();
    order.sort_by_key(|&index| breaks[index].min_quantity);
    for window in order.windows(2) {
        let first = &breaks[window[0]];
        let second = &breaks[window[1]];
        if first.min_quantity == second.min_quantity {
            return Err(OfferError::DuplicateTier {
                min: first.min_quantity.get(),
            });
        }
        if let Some(max) = first.max_quantity {
            if max >= second.min_quantity {
                return Err(OfferError::OverlappingTiers {
                    first_min: first.min_quantity.get(),
                    first_max: max.get(),
                    second_min: second.min_quantity.get(),
                });
            }
        }
    }
    Ok(())
}

/// A packaging option with its own commercial terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawPackagingOption", into = "RawPackagingOption")]
pub struct PackagingOption {
    /// The packaging kind.
    pub packaging: PackagingType,
    /// The source's label for this packaging.
    pub label: Option<Text<64>>,
    /// The MOQ of this packaging.
    pub moq: Option<NonZeroQuantity>,
    /// The order multiple of this packaging.
    pub order_multiple: Option<NonZeroQuantity>,
    /// The standard pack size of this packaging.
    pub standard_pack: Option<NonZeroQuantity>,
    /// The stock of this packaging.
    pub stock: StockState,
    /// The price tiers of this packaging.
    pub price_breaks: Vec<PriceBreak>,
    /// The lead time of this packaging.
    pub lead_time: Option<LeadTime>,
}

impl PackagingOption {
    /// Validate against the offer currency.
    pub fn validate(&self, currency: Currency) -> Result<(), OfferError> {
        validate_price_breaks(&self.price_breaks, currency)
    }
}

/// A purchasable variant (SKU) under one offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawVariantOffer", into = "RawVariantOffer")]
pub struct VariantOffer {
    /// The variant identifier.
    pub variant_id: VariantId,
    /// The variant's attributes.
    pub attributes: Vec<VariantAttribute>,
    /// The packaging of this variant, when stated.
    pub packaging: Option<PackagingType>,
    /// The variant's MOQ.
    pub moq: Option<NonZeroQuantity>,
    /// The variant's order multiple.
    pub order_multiple: Option<NonZeroQuantity>,
    /// The variant's standard pack.
    pub standard_pack: Option<NonZeroQuantity>,
    /// The variant's stock.
    pub stock: StockState,
    /// The variant's price tiers.
    pub price_breaks: Vec<PriceBreak>,
    /// The variant's lead time.
    pub lead_time: Option<LeadTime>,
}

impl VariantOffer {
    /// Validate against the offer currency.
    pub fn validate(&self, currency: Currency) -> Result<(), OfferError> {
        if self.attributes.len() > MAX_ATTRIBUTES_PER_VARIANT {
            return Err(OfferError::TooManyAttributes {
                max: MAX_ATTRIBUTES_PER_VARIANT,
                actual: self.attributes.len(),
            });
        }
        for (index, attribute) in self.attributes.iter().enumerate() {
            for other in self.attributes.iter().skip(index + 1) {
                if normalize_attribute(attribute.name.as_str())
                    == normalize_attribute(other.name.as_str())
                {
                    return Err(OfferError::DuplicateAttribute {
                        name: attribute.name.as_str().to_string(),
                    });
                }
            }
        }
        validate_price_breaks(&self.price_breaks, currency)
    }

    /// True when the variant matches every requested attribute.
    pub fn matches_attributes(&self, requested: &[VariantAttribute]) -> bool {
        requested.iter().all(|wanted| {
            self.attributes
                .iter()
                .any(|attribute| attribute.matches(wanted))
        })
    }
}

/// The lifecycle state of a part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    /// Active / in production.
    Active,
    /// Not recommended for new designs.
    NotRecommendedForNewDesign,
    /// Obsolete / end of life.
    Obsolete,
    /// Not stated by the source.
    #[default]
    Unknown,
}

/// The lifecycle ordering used by ranking (higher is better).
impl LifecycleStatus {
    /// A normalized `0..=1000` value, higher is better.
    pub const fn rank_normalized(self) -> i64 {
        match self {
            Self::Active => 1_000,
            Self::NotRecommendedForNewDesign => 400,
            Self::Obsolete => 100,
            Self::Unknown => 0,
        }
    }
}

/// A range of prices in one currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawPriceRange", into = "RawPriceRange")]
pub struct PriceRange {
    /// The lowest price.
    pub min: Money,
    /// The highest price.
    pub max: Money,
}

/// A rejected price range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PriceRangeError {
    /// The two bounds use different currencies.
    #[error("price range currencies differ: {left} vs {right}")]
    CurrencyMismatch {
        /// The first currency.
        left: Currency,
        /// The second currency.
        right: Currency,
    },
    /// The minimum is above the maximum.
    #[error("price range minimum {min} exceeds maximum {max}")]
    Inverted {
        /// The minimum.
        min: Money,
        /// The maximum.
        max: Money,
    },
}

impl PriceRange {
    /// Construct a validated range.
    pub fn new(min: Money, max: Money) -> Result<Self, PriceRangeError> {
        if min.currency != max.currency {
            return Err(PriceRangeError::CurrencyMismatch {
                left: min.currency,
                right: max.currency,
            });
        }
        if min.micros > max.micros {
            return Err(PriceRangeError::Inverted { min, max });
        }
        Ok(Self { min, max })
    }

    /// Widen the range to include `price` (same currency).
    pub fn widen(self, price: Money) -> Result<Self, PriceRangeError> {
        let min = if price.micros < self.min.micros {
            price
        } else {
            self.min
        };
        let max = if price.micros > self.max.micros {
            price
        } else {
            self.max
        };
        Self::new(min, max)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPriceRange {
    min: Money,
    max: Money,
}

impl TryFrom<RawPriceRange> for PriceRange {
    type Error = PriceRangeError;

    fn try_from(raw: RawPriceRange) -> Result<Self, Self::Error> {
        PriceRange::new(raw.min, raw.max)
    }
}

impl From<PriceRange> for RawPriceRange {
    fn from(range: PriceRange) -> Self {
        Self {
            min: range.min,
            max: range.max,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPriceBreak {
    min_quantity: NonZeroQuantity,
    max_quantity: Option<NonZeroQuantity>,
    unit_price: Money,
    visibility: PriceVisibility,
    account_scope: Option<AccountScope>,
    promotion: Option<Promotion>,
}

impl TryFrom<RawPriceBreak> for PriceBreak {
    type Error = OfferError;

    fn try_from(raw: RawPriceBreak) -> Result<Self, Self::Error> {
        let price_break = PriceBreak {
            min_quantity: raw.min_quantity,
            max_quantity: raw.max_quantity,
            unit_price: raw.unit_price,
            visibility: raw.visibility,
            account_scope: raw.account_scope,
            promotion: raw.promotion,
        };
        price_break.validate(price_break.unit_price.currency)?;
        Ok(price_break)
    }
}

impl From<PriceBreak> for RawPriceBreak {
    fn from(price_break: PriceBreak) -> Self {
        Self {
            min_quantity: price_break.min_quantity,
            max_quantity: price_break.max_quantity,
            unit_price: price_break.unit_price,
            visibility: price_break.visibility,
            account_scope: price_break.account_scope,
            promotion: price_break.promotion,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPackagingOption {
    packaging: PackagingType,
    label: Option<Text<64>>,
    moq: Option<NonZeroQuantity>,
    order_multiple: Option<NonZeroQuantity>,
    standard_pack: Option<NonZeroQuantity>,
    stock: StockState,
    price_breaks: Vec<PriceBreak>,
    lead_time: Option<LeadTime>,
}

impl TryFrom<RawPackagingOption> for PackagingOption {
    type Error = OfferError;

    fn try_from(raw: RawPackagingOption) -> Result<Self, Self::Error> {
        let option = PackagingOption {
            packaging: raw.packaging,
            label: raw.label,
            moq: raw.moq,
            order_multiple: raw.order_multiple,
            standard_pack: raw.standard_pack,
            stock: raw.stock,
            price_breaks: raw.price_breaks,
            lead_time: raw.lead_time,
        };
        for price_break in &option.price_breaks {
            price_break.validate(price_break.unit_price.currency)?;
        }
        Ok(option)
    }
}

impl From<PackagingOption> for RawPackagingOption {
    fn from(option: PackagingOption) -> Self {
        Self {
            packaging: option.packaging,
            label: option.label,
            moq: option.moq,
            order_multiple: option.order_multiple,
            standard_pack: option.standard_pack,
            stock: option.stock,
            price_breaks: option.price_breaks,
            lead_time: option.lead_time,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVariantOffer {
    variant_id: VariantId,
    attributes: Vec<VariantAttribute>,
    packaging: Option<PackagingType>,
    moq: Option<NonZeroQuantity>,
    order_multiple: Option<NonZeroQuantity>,
    standard_pack: Option<NonZeroQuantity>,
    stock: StockState,
    price_breaks: Vec<PriceBreak>,
    lead_time: Option<LeadTime>,
}

impl TryFrom<RawVariantOffer> for VariantOffer {
    type Error = OfferError;

    fn try_from(raw: RawVariantOffer) -> Result<Self, Self::Error> {
        let variant = VariantOffer {
            variant_id: raw.variant_id,
            attributes: raw.attributes,
            packaging: raw.packaging,
            moq: raw.moq,
            order_multiple: raw.order_multiple,
            standard_pack: raw.standard_pack,
            stock: raw.stock,
            price_breaks: raw.price_breaks,
            lead_time: raw.lead_time,
        };
        for price_break in &variant.price_breaks {
            price_break.validate(price_break.unit_price.currency)?;
        }
        Ok(variant)
    }
}

impl From<VariantOffer> for RawVariantOffer {
    fn from(variant: VariantOffer) -> Self {
        Self {
            variant_id: variant.variant_id,
            attributes: variant.attributes,
            packaging: variant.packaging,
            moq: variant.moq,
            order_multiple: variant.order_multiple,
            standard_pack: variant.standard_pack,
            stock: variant.stock,
            price_breaks: variant.price_breaks,
            lead_time: variant.lead_time,
        }
    }
}

/// A commercial offer as observed from one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawCommercialOffer", into = "RawCommercialOffer")]
pub struct CommercialOffer {
    /// The source that produced this offer.
    pub source: SourceId,
    /// The product identity.
    pub identity: ProductIdentity,
    /// The listing title.
    pub title: Text<512>,
    /// The listing description.
    pub description: Option<Text<4096>>,
    /// The offer currency (every price inside the offer uses it).
    pub currency: Currency,
    /// The offer-level price tiers.
    pub price_breaks: Vec<PriceBreak>,
    /// The variants (SKUs) of this offer.
    pub variants: Vec<VariantOffer>,
    /// The offer-level MOQ.
    pub moq: Option<NonZeroQuantity>,
    /// The offer-level order multiple.
    pub order_multiple: Option<NonZeroQuantity>,
    /// The offer-level standard pack.
    pub standard_pack: Option<NonZeroQuantity>,
    /// The offer-level stock.
    pub stock: StockState,
    /// The offer-level lead time.
    pub lead_time: Option<LeadTime>,
    /// The packaging options.
    pub packaging: Vec<PackagingOption>,
    /// The supplier.
    pub supplier: Option<Supplier>,
    /// The manufacturer.
    pub manufacturer: Option<Manufacturer>,
    /// The provenance of this observation.
    pub provenance: OfferProvenance,
    /// When the offer was observed (milliseconds since the Unix epoch).
    pub observed_at_ms: u64,
    /// The visibility of the offer-level price.
    pub price_visibility: PriceVisibility,
    /// The lifecycle state.
    pub lifecycle: LifecycleStatus,
}

impl CommercialOffer {
    /// Validate every cross-field invariant.
    pub fn validate(&self) -> Result<(), OfferError> {
        self.identity.validate()?;
        self.provenance.validate()?;
        if self.source != self.provenance.source {
            return Err(OfferError::SourceMismatch {
                offer: self.source.as_str().to_string(),
                provenance: self.provenance.source.as_str().to_string(),
            });
        }
        if self.variants.len() > MAX_VARIANTS_PER_OFFER {
            return Err(OfferError::TooManyVariants {
                max: MAX_VARIANTS_PER_OFFER,
                actual: self.variants.len(),
            });
        }
        if self.packaging.len() > MAX_PACKAGING_OPTIONS {
            return Err(OfferError::TooManyPackagingOptions {
                max: MAX_PACKAGING_OPTIONS,
                actual: self.packaging.len(),
            });
        }
        if matches!(
            self.price_visibility,
            PriceVisibility::InquiryRequired | PriceVisibility::Unknown
        ) {
            let has_price = !self.price_breaks.is_empty()
                || self
                    .variants
                    .iter()
                    .any(|variant| !variant.price_breaks.is_empty())
                || self
                    .packaging
                    .iter()
                    .any(|option| !option.price_breaks.is_empty());
            if has_price {
                return Err(OfferError::PriceWithInquiryVisibility);
            }
        }
        validate_price_breaks(&self.price_breaks, self.currency)?;
        for (index, variant) in self.variants.iter().enumerate() {
            variant.validate(self.currency)?;
            for other in self.variants.iter().skip(index + 1) {
                if variant.variant_id == other.variant_id {
                    return Err(OfferError::DuplicateVariantId {
                        id: variant.variant_id.as_str().to_string(),
                    });
                }
            }
        }
        for (index, option) in self.packaging.iter().enumerate() {
            option.validate(self.currency)?;
            for other in self.packaging.iter().skip(index + 1) {
                if option.packaging == other.packaging && option.label == other.label {
                    return Err(OfferError::DuplicatePackagingOption {
                        packaging: option.packaging,
                    });
                }
            }
        }
        Ok(())
    }

    /// The variant with `variant_id`, if any.
    pub fn variant_by_id(&self, variant_id: &str) -> Option<&VariantOffer> {
        self.variants
            .iter()
            .find(|variant| variant.variant_id.as_str() == variant_id)
    }

    /// All variants matching every requested attribute.
    pub fn variants_matching(&self, requested: &[VariantAttribute]) -> Vec<&VariantOffer> {
        self.variants
            .iter()
            .filter(|variant| variant.matches_attributes(requested))
            .collect()
    }

    /// The packaging options of one kind.
    pub fn packaging_options(&self, packaging: PackagingType) -> Vec<&PackagingOption> {
        self.packaging
            .iter()
            .filter(|option| option.packaging == packaging)
            .collect()
    }

    /// The lowest unit price anywhere in the offer (a headline figure, not
    /// a quote).
    pub fn cheapest_unit_price(&self) -> Option<Money> {
        self.price_range().map(|range| range.min)
    }

    /// The range of unit prices anywhere in the offer (a headline figure,
    /// not a quote).
    pub fn price_range(&self) -> Option<PriceRange> {
        let mut range: Option<PriceRange> = None;
        for price_break in self
            .price_breaks
            .iter()
            .chain(
                self.variants
                    .iter()
                    .flat_map(|variant| &variant.price_breaks),
            )
            .chain(
                self.packaging
                    .iter()
                    .flat_map(|option| &option.price_breaks),
            )
        {
            range = Some(match range {
                Some(existing) => existing.widen(price_break.unit_price).ok()?,
                None => PriceRange::new(price_break.unit_price, price_break.unit_price).ok()?,
            });
        }
        range
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommercialOffer {
    source: SourceId,
    identity: ProductIdentity,
    title: Text<512>,
    description: Option<Text<4096>>,
    currency: Currency,
    price_breaks: Vec<PriceBreak>,
    variants: Vec<VariantOffer>,
    moq: Option<NonZeroQuantity>,
    order_multiple: Option<NonZeroQuantity>,
    standard_pack: Option<NonZeroQuantity>,
    stock: StockState,
    lead_time: Option<LeadTime>,
    packaging: Vec<PackagingOption>,
    supplier: Option<Supplier>,
    manufacturer: Option<Manufacturer>,
    provenance: OfferProvenance,
    observed_at_ms: u64,
    price_visibility: PriceVisibility,
    #[serde(default)]
    lifecycle: LifecycleStatus,
}

impl TryFrom<RawCommercialOffer> for CommercialOffer {
    type Error = OfferError;

    fn try_from(raw: RawCommercialOffer) -> Result<Self, Self::Error> {
        let offer = CommercialOffer {
            source: raw.source,
            identity: raw.identity,
            title: raw.title,
            description: raw.description,
            currency: raw.currency,
            price_breaks: raw.price_breaks,
            variants: raw.variants,
            moq: raw.moq,
            order_multiple: raw.order_multiple,
            standard_pack: raw.standard_pack,
            stock: raw.stock,
            lead_time: raw.lead_time,
            packaging: raw.packaging,
            supplier: raw.supplier,
            manufacturer: raw.manufacturer,
            provenance: raw.provenance,
            observed_at_ms: raw.observed_at_ms,
            price_visibility: raw.price_visibility,
            lifecycle: raw.lifecycle,
        };
        offer.validate()?;
        Ok(offer)
    }
}

impl From<CommercialOffer> for RawCommercialOffer {
    fn from(offer: CommercialOffer) -> Self {
        Self {
            source: offer.source,
            identity: offer.identity,
            title: offer.title,
            description: offer.description,
            currency: offer.currency,
            price_breaks: offer.price_breaks,
            variants: offer.variants,
            moq: offer.moq,
            order_multiple: offer.order_multiple,
            standard_pack: offer.standard_pack,
            stock: offer.stock,
            lead_time: offer.lead_time,
            packaging: offer.packaging,
            supplier: offer.supplier,
            manufacturer: offer.manufacturer,
            provenance: offer.provenance,
            observed_at_ms: offer.observed_at_ms,
            price_visibility: offer.price_visibility,
            lifecycle: offer.lifecycle,
        }
    }
}

/// A rejected offer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfferError {
    /// The identity has no identifier.
    #[error(transparent)]
    Identity(#[from] crate::identity::IdentityError),
    /// A nested money value uses a different currency.
    #[error("{field} uses {actual} but the offer currency is {expected}")]
    CurrencyMismatch {
        /// The field path.
        field: &'static str,
        /// The offer currency.
        expected: Currency,
        /// The actual currency.
        actual: Currency,
    },
    /// A negative price.
    #[error("{field} is negative")]
    NegativePrice {
        /// The field path.
        field: &'static str,
    },
    /// A price exists although visibility says there is none.
    #[error("price present although price visibility is inquiry_required")]
    PriceWithInquiryVisibility,
    /// A tier range with `max < min`.
    #[error("tier range {min}..={max} is inverted")]
    InvalidTierRange {
        /// The minimum.
        min: u64,
        /// The maximum.
        max: u64,
    },
    /// Two tiers share a minimum quantity.
    #[error("duplicate price tier minimum {min}")]
    DuplicateTier {
        /// The duplicated minimum.
        min: u64,
    },
    /// Two tiers overlap.
    #[error("tiers overlap: {first_min}..={first_max} vs {second_min}..")]
    OverlappingTiers {
        /// The first tier's minimum.
        first_min: u64,
        /// The first tier's maximum.
        first_max: u64,
        /// The second tier's minimum.
        second_min: u64,
    },
    /// Too many variants.
    #[error("offer has {actual} variants; the maximum is {max}")]
    TooManyVariants {
        /// The bound.
        max: usize,
        /// The actual count.
        actual: usize,
    },
    /// Too many price breaks.
    #[error("price list has {actual} tiers; the maximum is {max}")]
    TooManyPriceBreaks {
        /// The bound.
        max: usize,
        /// The actual count.
        actual: usize,
    },
    /// Too many attributes on one variant.
    #[error("variant has {actual} attributes; the maximum is {max}")]
    TooManyAttributes {
        /// The bound.
        max: usize,
        /// The actual count.
        actual: usize,
    },
    /// Too many packaging options.
    #[error("offer has {actual} packaging options; the maximum is {max}")]
    TooManyPackagingOptions {
        /// The bound.
        max: usize,
        /// The actual count.
        actual: usize,
    },
    /// Two variants share an id.
    #[error("duplicate variant id {id:?}")]
    DuplicateVariantId {
        /// The duplicated id.
        id: String,
    },
    /// Two attributes of one variant share a name.
    #[error("duplicate variant attribute {name:?}")]
    DuplicateAttribute {
        /// The duplicated name.
        name: String,
    },
    /// Two packaging options are identical.
    #[error("duplicate packaging option {packaging}")]
    DuplicatePackagingOption {
        /// The duplicated packaging.
        packaging: PackagingType,
    },
    /// A promotion is not applicable to its tier.
    #[error("invalid promotion: {reason}")]
    InvalidPromotion {
        /// Why the promotion was rejected.
        reason: String,
    },
    /// An inverted lead time.
    #[error("lead time {min_days}..{max_days} is inverted")]
    InvalidLeadTime {
        /// The minimum days.
        min_days: u16,
        /// The maximum days.
        max_days: u16,
    },
    /// A malformed content digest.
    #[error("content digest {value:?} is not 64 hex characters")]
    InvalidDigest {
        /// The rejected digest.
        value: String,
    },
    /// The offer source and the provenance source disagree.
    #[error("offer source {offer:?} does not match provenance source {provenance:?}")]
    SourceMismatch {
        /// The offer-level source.
        offer: String,
        /// The provenance source.
        provenance: String,
    },
    /// An account-specific tier without an account scope.
    #[error("account-specific price requires an account scope")]
    AccountScopeRequired,
    /// An account scope on a non-account-specific price.
    #[error("account scope is only valid for account_specific or promotional prices")]
    AccountScopeUnexpected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantity::Quantity;
    use crate::text::VariantId;

    fn provenance() -> OfferProvenance {
        OfferProvenance {
            origin: ObservationOrigin::OfficialApi,
            source: SourceId::new("lcsc").expect("source"),
            extractor_version: None,
            connector_version: None,
            normalization_version: None,
            content_digest: None,
            account_scope: None,
            locale: None,
            market: None,
            source_confidence_bp: None,
        }
    }

    fn identity() -> ProductIdentity {
        ProductIdentity {
            manufacturer: Some(Text::new("STMicroelectronics").expect("text")),
            manufacturer_part_number: Some(Text::new("STM32F407VGT6").expect("text")),
            source_part_number: None,
            offer_id: Some(Text::new("offer-1").expect("text")),
            canonical_url: None,
            category: None,
        }
    }

    fn offer() -> CommercialOffer {
        CommercialOffer {
            source: SourceId::new("lcsc").expect("source"),
            identity: identity(),
            title: Text::new("STM32F407VGT6").expect("title"),
            description: None,
            currency: Currency::CNY,
            price_breaks: Vec::new(),
            variants: Vec::new(),
            moq: None,
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            lead_time: None,
            packaging: Vec::new(),
            supplier: None,
            manufacturer: None,
            provenance: provenance(),
            observed_at_ms: 1,
            price_visibility: PriceVisibility::Public,
            lifecycle: LifecycleStatus::Unknown,
        }
    }

    #[test]
    fn freshness_never_presents_stale_as_current() {
        assert!(Freshness::Live.is_current());
        assert!(!Freshness::Live.is_stale());
        assert_eq!(Freshness::Live.age_ms(), None);
        let fresh = Freshness::FreshCache { age_ms: 60_000 };
        assert!(fresh.is_current());
        assert!(!fresh.is_stale());
        assert_eq!(fresh.age_ms(), Some(60_000));
        let stale = Freshness::StaleFallback { age_ms: 86_400_000 };
        assert!(!stale.is_current());
        assert!(stale.is_stale());
        assert_eq!(stale.age_ms(), Some(86_400_000));

        assert_eq!(
            serde_json::to_value(Freshness::FreshCache { age_ms: 1 }).expect("serialize"),
            serde_json::json!({"state": "fresh_cache", "age_ms": 1})
        );
        assert_eq!(
            serde_json::to_value(Freshness::Live).expect("serialize"),
            serde_json::json!({"state": "live"})
        );
        assert!(serde_json::from_str::<Freshness>(r#"{"state":"fresh_cache"}"#).is_err());
        assert!(
            serde_json::from_str::<Freshness>(r#"{"state":"stale_fallback","age_ms":-1}"#).is_err()
        );
    }

    #[test]
    fn stock_states_are_typed_and_zero_is_not_in_stock() {
        assert_eq!(
            StockState::InStock {
                quantity: NonZeroQuantity::new(5).expect("qty")
            }
            .available_quantity(),
            Some(5)
        );
        assert_eq!(StockState::OutOfStock.available_quantity(), None);
        assert_eq!(StockState::Unknown.available_quantity(), None);
        assert!(!StockState::Unknown.is_known());
        assert!(StockState::OutOfStock.is_known());
        assert!(
            serde_json::from_str::<StockState>(r#"{"state":"in_stock","quantity":0}"#).is_err()
        );
        assert!(serde_json::from_str::<StockState>(r#"{"state":"in_stock"}"#).is_err());
        assert!(serde_json::from_str::<StockState>(r#"{"state":"plenty"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<StockState>(
                r#"{"state":"backorder","quantity":null,"lead_time":null}"#
            )
            .expect("valid"),
            StockState::Backorder {
                quantity: None,
                lead_time: None
            }
        );
    }

    #[test]
    fn lead_time_ranges_are_validated() {
        assert_eq!(
            LeadTime::new(3, 5).expect("valid"),
            LeadTime {
                min_days: 3,
                max_days: 5
            }
        );
        assert!(matches!(
            LeadTime::new(5, 3),
            Err(OfferError::InvalidLeadTime { .. })
        ));
        assert_eq!(
            serde_json::from_str::<LeadTime>(r#"{"min_days":3,"max_days":3}"#).expect("valid"),
            LeadTime::new(3, 3).expect("valid")
        );
        assert!(serde_json::from_str::<LeadTime>(r#"{"min_days":5,"max_days":3}"#).is_err());
        assert!(serde_json::from_str::<LeadTime>(r#"{"min_days":1}"#).is_err());
        assert!(
            serde_json::from_str::<LeadTime>(r#"{"min_days":1,"max_days":2,"note":"x"}"#).is_err()
        );
    }

    #[test]
    fn price_visibility_wire_labels_are_stable() {
        for (visibility, label) in [
            (PriceVisibility::Public, "\"public\""),
            (PriceVisibility::Authenticated, "\"authenticated\""),
            (PriceVisibility::AccountSpecific, "\"account_specific\""),
            (PriceVisibility::Promotional, "\"promotional\""),
            (PriceVisibility::InquiryRequired, "\"inquiry_required\""),
            (PriceVisibility::Unknown, "\"unknown\""),
        ] {
            assert_eq!(
                serde_json::to_string(&visibility).expect("serialize"),
                label
            );
            assert_eq!(
                serde_json::from_str::<PriceVisibility>(label).expect("round trip"),
                visibility
            );
        }
        assert!(serde_json::from_str::<PriceVisibility>("\"cheap\"").is_err());
    }

    #[test]
    fn lifecycle_defaults_to_unknown_and_ranks_by_evidence() {
        assert_eq!(LifecycleStatus::default(), LifecycleStatus::Unknown);
        assert_eq!(
            serde_json::from_str::<LifecycleStatus>("\"not_recommended_for_new_design\"")
                .expect("valid"),
            LifecycleStatus::NotRecommendedForNewDesign
        );
        assert!(
            LifecycleStatus::Active.rank_normalized()
                > LifecycleStatus::NotRecommendedForNewDesign.rank_normalized()
        );
        assert!(
            LifecycleStatus::NotRecommendedForNewDesign.rank_normalized()
                > LifecycleStatus::Obsolete.rank_normalized()
        );
        assert!(
            LifecycleStatus::Obsolete.rank_normalized()
                > LifecycleStatus::Unknown.rank_normalized()
        );
    }

    #[test]
    fn price_range_is_validated_and_widened_exactly() {
        let range = PriceRange::new(
            Money::from_micros(Currency::CNY, 100),
            Money::from_micros(Currency::CNY, 200),
        )
        .expect("valid");
        let widened = range
            .widen(Money::from_micros(Currency::CNY, 50))
            .expect("widen");
        assert_eq!(widened.min.micros, 50);
        assert_eq!(widened.max.micros, 200);
        assert!(matches!(
            PriceRange::new(
                Money::from_micros(Currency::CNY, 200),
                Money::from_micros(Currency::CNY, 100)
            ),
            Err(PriceRangeError::Inverted { .. })
        ));
        assert!(matches!(
            PriceRange::new(
                Money::from_micros(Currency::CNY, 1),
                Money::from_micros(Currency::USD, 2)
            ),
            Err(PriceRangeError::CurrencyMismatch { .. })
        ));
        assert!(serde_json::from_str::<PriceRange>(
            r#"{"min":{"currency":"CNY","amount":"2.000000"},"max":{"currency":"CNY","amount":"1.000000"}}"#
        )
        .is_err());
    }

    #[test]
    fn provenance_validates_digest_shape() {
        let mut provenance = provenance();
        provenance.content_digest = Some(Text::new(&"a".repeat(64)).expect("digest"));
        assert!(provenance.validate().is_ok());
        provenance.content_digest =
            Some(Text::new(&format!("blake3:{}", "0f".repeat(32))).expect("digest"));
        assert!(provenance.validate().is_ok());
        provenance.content_digest = Some(Text::new("deadbeef").expect("digest"));
        assert!(matches!(
            provenance.validate(),
            Err(OfferError::InvalidDigest { .. })
        ));
        provenance.content_digest = Some(Text::new(&"z".repeat(64)).expect("digest"));
        assert!(provenance.validate().is_err());
    }

    #[test]
    fn confidence_is_bounded() {
        assert_eq!(ConfidenceBp::new(10_000).expect("valid").get(), 10_000);
        assert!(ConfidenceBp::new(10_001).is_err());
        assert!(serde_json::from_str::<ConfidenceBp>("10000").is_ok());
        assert!(serde_json::from_str::<ConfidenceBp>("10001").is_err());
        assert!(serde_json::from_str::<ConfidenceBp>("-1").is_err());
        assert!(serde_json::from_str::<ConfidenceBp>("\"high\"").is_err());
    }

    #[test]
    fn seller_completeness_counts_evidence() {
        let supplier = Supplier {
            id: None,
            name: Text::new("Shenzhen Components").expect("name"),
            country: None,
            url: None,
            account_scope: None,
        };
        assert_eq!(supplier.completeness_bp(), 0);
        let manufacturer = Manufacturer {
            id: Some(Text::new("st").expect("id")),
            name: Text::new("STMicroelectronics").expect("name"),
            country: Some(Text::new("CH").expect("country")),
            url: None,
        };
        assert_eq!(manufacturer.completeness_bp(), 7_500);
    }

    #[test]
    fn offer_helpers_never_fabricate_a_price() {
        let mut offer = offer();
        offer.price_breaks = vec![PriceBreak {
            min_quantity: NonZeroQuantity::new(100).expect("qty"),
            max_quantity: None,
            unit_price: Money::from_micros(Currency::CNY, 18_200_000),
            visibility: PriceVisibility::Public,
            account_scope: None,
            promotion: None,
        }];
        assert_eq!(
            offer.cheapest_unit_price(),
            Some(Money::from_micros(Currency::CNY, 18_200_000))
        );
        assert_eq!(
            offer.price_range().expect("range").min,
            Money::from_micros(Currency::CNY, 18_200_000)
        );

        let mut variant = VariantOffer {
            variant_id: VariantId::new("v1").expect("id"),
            attributes: vec![VariantAttribute {
                name: Text::new("grade").expect("name"),
                value: Text::new("Industrial").expect("value"),
            }],
            packaging: Some(PackagingType::Tray),
            moq: None,
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            price_breaks: vec![PriceBreak {
                min_quantity: NonZeroQuantity::new(1).expect("qty"),
                max_quantity: None,
                unit_price: Money::from_micros(Currency::CNY, 36_000_000),
                visibility: PriceVisibility::Public,
                account_scope: None,
                promotion: None,
            }],
            lead_time: None,
        };
        variant.attributes.push(VariantAttribute {
            name: Text::new("package").expect("name"),
            value: Text::new("LQFP-100").expect("value"),
        });
        offer.variants = vec![variant];
        offer.packaging = vec![PackagingOption {
            packaging: PackagingType::Tray,
            label: None,
            moq: Some(NonZeroQuantity::new(100).expect("qty")),
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            price_breaks: Vec::new(),
            lead_time: None,
        }];

        assert_eq!(
            offer.variant_by_id("v1").expect("variant").packaging,
            Some(PackagingType::Tray)
        );
        assert!(offer.variant_by_id("missing").is_none());
        assert_eq!(
            offer
                .variants_matching(&[VariantAttribute {
                    name: Text::new("PACKAGE").expect("name"),
                    value: Text::new("LQFP-100").expect("value"),
                }])
                .len(),
            1,
            "attribute matching is case- and space-insensitive"
        );
        assert!(
            offer
                .variants_matching(&[VariantAttribute {
                    name: Text::new("package").expect("name"),
                    value: Text::new("LQFP100").expect("value"),
                }])
                .is_empty(),
            "attribute values keep significant separators: prefer false negatives"
        );
        assert!(offer
            .variants_matching(&[VariantAttribute {
                name: Text::new("grade").expect("name"),
                value: Text::new("automotive").expect("value"),
            }])
            .is_empty());
        assert_eq!(offer.packaging_options(PackagingType::Tray).len(), 1);
        assert!(offer.packaging_options(PackagingType::Tray)[0]
            .price_breaks
            .is_empty());
        assert_eq!(offer.validate(), Ok(()));
    }

    #[test]
    fn tier_applicability_and_account_scope_rules() {
        let scope = AccountScope::new("acct-1").expect("scope");
        let mut price_break = PriceBreak {
            min_quantity: NonZeroQuantity::new(100).expect("qty"),
            max_quantity: Some(NonZeroQuantity::new(999).expect("qty")),
            unit_price: Money::from_micros(Currency::CNY, 1),
            visibility: PriceVisibility::AccountSpecific,
            account_scope: Some(scope.clone()),
            promotion: None,
        };
        assert!(price_break.applies_at(Quantity::new(100).expect("qty"), Some(&scope)));
        assert!(price_break.applies_at(Quantity::new(999).expect("qty"), Some(&scope)));
        assert!(!price_break.applies_at(Quantity::new(1000).expect("qty"), Some(&scope)));
        assert!(!price_break.applies_at(Quantity::new(500).expect("qty"), None));
        assert!(!price_break.applies_at(
            Quantity::new(500).expect("qty"),
            Some(&AccountScope::new("other").expect("scope"))
        ));
        assert!(price_break.is_account_specific());

        let mut missing_scope = price_break.clone();
        missing_scope.account_scope = None;
        assert!(matches!(
            missing_scope.validate(Currency::CNY),
            Err(OfferError::AccountScopeRequired)
        ));

        price_break.visibility = PriceVisibility::Public;
        assert!(matches!(
            price_break.validate(Currency::CNY),
            Err(OfferError::AccountScopeUnexpected)
        ));

        price_break.visibility = PriceVisibility::InquiryRequired;
        price_break.account_scope = None;
        assert!(matches!(
            price_break.validate(Currency::CNY),
            Err(OfferError::PriceWithInquiryVisibility)
        ));

        price_break.visibility = PriceVisibility::Public;
        price_break.unit_price = Money::from_micros(Currency::CNY, -1);
        assert!(matches!(
            price_break.validate(Currency::CNY),
            Err(OfferError::NegativePrice { .. })
        ));
    }

    #[test]
    fn promotions_are_validated_against_their_tier() {
        let unit_price = Money::from_micros(Currency::CNY, 1_000_000);
        let mut price_break = PriceBreak {
            min_quantity: NonZeroQuantity::new(1).expect("qty"),
            max_quantity: None,
            unit_price,
            visibility: PriceVisibility::Public,
            account_scope: None,
            promotion: None,
        };
        price_break.promotion = Some(Promotion {
            id: Text::new("p1").expect("id"),
            kind: PromotionKind::AmountOff {
                per_unit: Money::from_micros(Currency::CNY, 2_000_000),
            },
            valid_until_ms: None,
            label: None,
        });
        assert!(matches!(
            price_break.validate(Currency::CNY),
            Err(OfferError::InvalidPromotion { .. })
        ));

        price_break.promotion = Some(Promotion {
            id: Text::new("p2").expect("id"),
            kind: PromotionKind::AmountOff {
                per_unit: Money::from_micros(Currency::USD, 1),
            },
            valid_until_ms: None,
            label: None,
        });
        assert!(matches!(
            price_break.validate(Currency::CNY),
            Err(OfferError::CurrencyMismatch { .. })
        ));

        price_break.promotion = Some(Promotion {
            id: Text::new("p3").expect("id"),
            kind: PromotionKind::BundlePrice {
                quantity: NonZeroQuantity::new(100).expect("qty"),
                price: Money::from_micros(Currency::CNY, 900_000),
            },
            valid_until_ms: Some(5),
            label: None,
        });
        assert!(price_break.validate(Currency::CNY).is_ok());
        assert!(price_break
            .promotion
            .as_ref()
            .expect("promotion")
            .is_active_at(4));
        assert!(!price_break
            .promotion
            .as_ref()
            .expect("promotion")
            .is_active_at(5));
    }

    #[test]
    fn offer_source_must_match_provenance_source() {
        let mut offer = offer();
        offer.provenance.source = SourceId::new("mouser").expect("source");
        assert!(matches!(
            offer.validate(),
            Err(OfferError::SourceMismatch { .. })
        ));
    }

    #[test]
    fn offer_identity_must_carry_an_identifier() {
        let mut offer = offer();
        offer.identity = ProductIdentity::empty();
        assert!(matches!(
            offer.validate(),
            Err(OfferError::Identity(
                crate::identity::IdentityError::NoIdentifier
            ))
        ));
    }

    #[test]
    fn variant_and_attribute_bounds_are_validated() {
        let mut variant = VariantOffer {
            variant_id: VariantId::new("v1").expect("id"),
            attributes: Vec::new(),
            packaging: None,
            moq: None,
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            price_breaks: Vec::new(),
            lead_time: None,
        };
        variant.attributes = (0..(MAX_ATTRIBUTES_PER_VARIANT + 1))
            .map(|index| VariantAttribute {
                name: Text::new(&format!("a{index}")).expect("name"),
                value: Text::new("x").expect("value"),
            })
            .collect();
        assert!(matches!(
            variant.validate(Currency::CNY),
            Err(OfferError::TooManyAttributes { .. })
        ));
    }
}
