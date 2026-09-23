//! Transparent, deterministic ranking.
//!
//! Ranking is a weighted integer function over named, machine-visible
//! components. There is no hidden model, no floating point and no
//! input-order dependence: [`rank_offers`] sorts by total score, then by
//! documented deterministic tie-breakers (source, part number, observed
//! time, title, input index), so the same inputs always produce the same
//! order regardless of the order they were supplied in.
//!
//! Every component normalizes to `0..=1000` and contributes
//! `weight * normalized / 1000` (floor). A component whose input is
//! *unknown* contributes 0 — evidence ranks higher than absence of
//! evidence, never the other way around — while a component whose
//! constraint simply does not apply (no MOQ, no order multiple, no
//! requested variant attributes) contributes full credit.

use crate::matching::{match_offer, PartQuery};
use crate::offer::{CommercialOffer, StockState};
use crate::quantity::Quantity;
use crate::quote::{
    effective_terms, resolve_quote, selected_scope, PricingContext, QuoteStatus, VariantRequest,
};
use crate::text::AccountScope;

/// A named ranking component.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ScoreKey {
    /// Exact MPN.
    ExactMpn,
    /// Manufacturer agreement.
    Manufacturer,
    /// Package agreement.
    Package,
    /// Quantity compatibility.
    QuantityCompatibility,
    /// Stock evidence.
    Stock,
    /// MOQ compatibility.
    Moq,
    /// Order-multiple compatibility.
    OrderMultiple,
    /// Lifecycle state.
    Lifecycle,
    /// Price completeness.
    PriceCompleteness,
    /// Source confidence.
    SourceConfidence,
    /// Seller (supplier/manufacturer) completeness.
    SellerCompleteness,
    /// Title token overlap (marketplace signal).
    TitleTokenOverlap,
    /// Variant/spec overlap (marketplace signal).
    VariantOverlap,
    /// Supplier evidence (marketplace signal).
    SupplierEvidence,
}

impl ScoreKey {
    /// The documented weight of this component.
    pub const fn weight(self) -> i64 {
        match self {
            Self::ExactMpn => 1_000,
            Self::Manufacturer => 300,
            Self::Package => 200,
            Self::QuantityCompatibility => 200,
            Self::Stock => 150,
            Self::Moq => 100,
            Self::OrderMultiple => 60,
            Self::Lifecycle => 120,
            Self::PriceCompleteness => 200,
            Self::SourceConfidence => 80,
            Self::SellerCompleteness => 60,
            Self::TitleTokenOverlap => 100,
            Self::VariantOverlap => 80,
            Self::SupplierEvidence => 50,
        }
    }

    /// The wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactMpn => "exact_mpn",
            Self::Manufacturer => "manufacturer",
            Self::Package => "package",
            Self::QuantityCompatibility => "quantity_compatibility",
            Self::Stock => "stock",
            Self::Moq => "moq",
            Self::OrderMultiple => "order_multiple",
            Self::Lifecycle => "lifecycle",
            Self::PriceCompleteness => "price_completeness",
            Self::SourceConfidence => "source_confidence",
            Self::SellerCompleteness => "seller_completeness",
            Self::TitleTokenOverlap => "title_token_overlap",
            Self::VariantOverlap => "variant_overlap",
            Self::SupplierEvidence => "supplier_evidence",
        }
    }
}

/// One component of a ranking score.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScoreComponent {
    /// The component.
    pub key: ScoreKey,
    /// The documented weight.
    pub weight: i64,
    /// The normalized input (`0..=1000`).
    pub normalized: i64,
    /// `weight * normalized / 1000`.
    pub contribution: i64,
}

/// A fully transparent score.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankScore {
    /// The sum of all component contributions.
    pub total: i64,
    /// Every component, in a fixed order.
    pub components: Vec<ScoreComponent>,
}

/// One ranked offer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RankedOffer<'a> {
    /// The 1-based rank.
    pub rank: usize,
    /// The index of the offer in the input slice.
    pub offer_index: usize,
    /// The transparent score.
    pub score: RankScore,
    /// The offer.
    pub offer: &'a CommercialOffer,
}

/// The inputs ranking may depend on besides the offer and the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankingContext {
    /// The quantity the caller intends to buy.
    pub quantity: Quantity,
    /// The required packaging.
    pub packaging: Option<crate::packaging::PackagingType>,
    /// The account scope (account pricing is never applied without it).
    pub account: Option<AccountScope>,
    /// The time used for promotion expiry.
    pub now_ms: u64,
    /// Whether promotions are applied.
    pub apply_promotions: bool,
}

impl RankingContext {
    /// A context for one quantity with no account and no packaging.
    pub fn at_quantity(quantity: Quantity) -> Self {
        Self {
            quantity,
            packaging: None,
            account: None,
            now_ms: 0,
            apply_promotions: true,
        }
    }
}

fn component(key: ScoreKey, normalized: i64) -> ScoreComponent {
    let normalized = normalized.clamp(0, 1_000);
    let weight = key.weight();
    ScoreComponent {
        key,
        weight,
        normalized,
        contribution: weight * normalized / 1_000,
    }
}

fn stock_component(stock: StockState, quantity: Quantity) -> ScoreComponent {
    let normalized = match stock {
        StockState::InStock {
            quantity: available,
        } => {
            if available.get() >= quantity.as_u64() {
                1_000
            } else {
                500
            }
        }
        StockState::Backorder { .. } => 200,
        StockState::OutOfStock => 0,
        StockState::Unknown => 0,
    };
    component(ScoreKey::Stock, normalized)
}

fn moq_component(
    moq: Option<crate::quantity::NonZeroQuantity>,
    quantity: Quantity,
) -> ScoreComponent {
    let normalized = match moq {
        Some(moq) if quantity < moq.as_quantity() => 0,
        _ => 1_000,
    };
    component(ScoreKey::Moq, normalized)
}

fn multiple_component(
    order_multiple: Option<crate::quantity::NonZeroQuantity>,
    quantity: Quantity,
) -> ScoreComponent {
    let normalized = match order_multiple {
        Some(multiple) if !quantity.as_u64().is_multiple_of(multiple.get()) => 500,
        _ => 1_000,
    };
    component(ScoreKey::OrderMultiple, normalized)
}

fn seller_completeness(offer: &CommercialOffer) -> ScoreComponent {
    let supplier = offer
        .supplier
        .as_ref()
        .map(|supplier| supplier.completeness_bp())
        .unwrap_or(0) as i64;
    let manufacturer = offer
        .manufacturer
        .as_ref()
        .map(|manufacturer| manufacturer.completeness_bp())
        .unwrap_or(0) as i64;
    component(ScoreKey::SellerCompleteness, (supplier + manufacturer) / 20)
}

fn score_offer(query: &PartQuery, offer: &CommercialOffer, ctx: &RankingContext) -> RankScore {
    let signals = match_offer(query, offer, 0).signals;

    // `exact_mpn` normalizes the strength of the part-number relation:
    // identical canonical form 1000, equal base with only packaging
    // suffixes 600, equal base with other suffix differences 300, a source
    // part-number match 250, no relation 0.
    let mpn_normalized = if signals.mpn_exact {
        1_000
    } else if signals.mpn_base_equal {
        let packaging_only = !signals.suffix_kinds.is_empty()
            && signals
                .suffix_kinds
                .iter()
                .all(|kind| matches!(kind, crate::identity::SuffixKind::Packaging(_)));
        if packaging_only {
            600
        } else {
            300
        }
    } else if signals.source_part_exact {
        250
    } else {
        0
    };
    let exact_mpn = component(ScoreKey::ExactMpn, mpn_normalized);
    let manufacturer = component(
        ScoreKey::Manufacturer,
        if signals.manufacturer_exact {
            1_000
        } else if signals.manufacturer_conflict {
            0
        } else if signals.manufacturer_known {
            500
        } else {
            0
        },
    );
    let package = component(
        ScoreKey::Package,
        match signals.package_match {
            Some(true) => 1_000,
            Some(false) => 0,
            None => 0,
        },
    );

    let variant_request = if query.attributes.is_empty() {
        VariantRequest::None
    } else {
        VariantRequest::Attributes(&query.attributes)
    };
    let pricing_ctx = PricingContext {
        variant: variant_request,
        packaging: ctx.packaging,
        account: ctx.account.as_ref(),
        now_ms: ctx.now_ms,
        apply_promotions: ctx.apply_promotions,
        shipping: None,
        tax: None,
        duty: None,
    };
    // MOQ, order multiple and stock are scored from the SELECTED
    // variant/packaging scope when the context selects one, exactly like the
    // price; offer-level terms only apply when no scope was selected.
    let (scope_variant, scope_packaging) = selected_scope(offer, &pricing_ctx);
    let scoped_terms = effective_terms(offer, scope_variant, scope_packaging);
    let resolution = resolve_quote(offer, &pricing_ctx, ctx.quantity);
    let quantity_compatibility = component(
        ScoreKey::QuantityCompatibility,
        match resolution.status {
            QuoteStatus::Resolved => 1_000,
            QuoteStatus::VariantAmbiguous | QuoteStatus::PackagingAmbiguous => 500,
            _ => 0,
        },
    );
    let has_prices = !offer.price_breaks.is_empty()
        || offer
            .variants
            .iter()
            .any(|variant| !variant.price_breaks.is_empty())
        || offer
            .packaging
            .iter()
            .any(|option| !option.price_breaks.is_empty());
    let price_completeness = component(
        ScoreKey::PriceCompleteness,
        match resolution.status {
            QuoteStatus::Resolved => 1_000,
            _ if has_prices => 400,
            _ => 0,
        },
    );
    let lifecycle = component(ScoreKey::Lifecycle, offer.lifecycle.rank_normalized());
    let source_confidence = component(
        ScoreKey::SourceConfidence,
        offer
            .provenance
            .source_confidence_bp
            .map(|confidence| i64::from(confidence.get()) / 10)
            .unwrap_or(0),
    );
    let title_overlap = component(
        ScoreKey::TitleTokenOverlap,
        i64::from(signals.title_token_overlap_bp.unwrap_or(0)) / 10,
    );
    let variant_overlap = component(
        ScoreKey::VariantOverlap,
        match signals.spec_overlap_bp {
            Some(overlap) => i64::from(overlap) / 10,
            None => 1_000,
        },
    );
    let supplier_evidence = component(
        ScoreKey::SupplierEvidence,
        if signals.supplier_evidence { 1_000 } else { 0 },
    );

    let components = vec![
        exact_mpn,
        manufacturer,
        package,
        quantity_compatibility,
        stock_component(scoped_terms.stock, ctx.quantity),
        moq_component(scoped_terms.moq, ctx.quantity),
        multiple_component(scoped_terms.order_multiple, ctx.quantity),
        lifecycle,
        price_completeness,
        source_confidence,
        seller_completeness(offer),
        title_overlap,
        variant_overlap,
        supplier_evidence,
    ];
    let total = components
        .iter()
        .map(|component| component.contribution)
        .sum();
    RankScore { total, components }
}

fn tie_break_key(offer: &CommercialOffer) -> (String, String, std::cmp::Reverse<u64>, String) {
    (
        offer.source.as_str().to_string(),
        offer
            .identity
            .manufacturer_part_number
            .as_ref()
            .map(|value| value.as_str().to_string())
            .or_else(|| {
                offer
                    .identity
                    .source_part_number
                    .as_ref()
                    .map(|value| value.as_str().to_string())
            })
            .or_else(|| {
                offer
                    .identity
                    .offer_id
                    .as_ref()
                    .map(|value| value.as_str().to_string())
            })
            .unwrap_or_default(),
        std::cmp::Reverse(offer.observed_at_ms),
        offer.title.as_str().to_string(),
    )
}

/// Rank offers against a query. The returned order is deterministic and
/// independent of the input order.
pub fn rank_offers<'a>(
    query: &PartQuery,
    offers: &'a [CommercialOffer],
    ctx: &RankingContext,
) -> Vec<RankedOffer<'a>> {
    let mut scored: Vec<(RankScore, usize, &'a CommercialOffer)> = offers
        .iter()
        .enumerate()
        .map(|(index, offer)| (score_offer(query, offer, ctx), index, offer))
        .collect();
    scored.sort_by(|left, right| {
        right
            .0
            .total
            .cmp(&left.0.total)
            .then_with(|| tie_break_key(left.2).cmp(&tie_break_key(right.2)))
            .then_with(|| left.1.cmp(&right.1))
    });
    scored
        .into_iter()
        .enumerate()
        .map(|(rank, (score, offer_index, offer))| RankedOffer {
            rank: rank + 1,
            offer_index,
            score,
            offer,
        })
        .collect()
}
