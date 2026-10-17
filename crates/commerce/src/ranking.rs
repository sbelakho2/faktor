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

use crate::matching::{match_signals_in_view, PartQuery, QueryView};
use crate::offer::{CommercialOffer, StockState};
use crate::quantity::Quantity;
use crate::quote::{
    effective_terms, resolve_quote_scoped, selected_scope, PricingContext, QuoteStatus,
    VariantRequest,
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

fn score_offer(view: &QueryView<'_>, offer: &CommercialOffer, ctx: &RankingContext) -> RankScore {
    let query = view.query();
    let signals = match_signals_in_view(view, offer);

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
    let (resolution, resolved_variant, resolved_packaging) =
        resolve_quote_scoped(offer, &pricing_ctx, ctx.quantity);
    // A resolved quote already carries the variant/packaging it priced;
    // reuse that scope instead of re-running variant matching. Refused or
    // ambiguous quotes keep the exact pre-fix `selected_scope` behavior.
    let (scope_variant, scope_packaging) = if resolution.status == QuoteStatus::Resolved {
        (resolved_variant, resolved_packaging)
    } else {
        selected_scope(offer, &pricing_ctx)
    };
    let scoped_terms = effective_terms(offer, scope_variant, scope_packaging);
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
    let view = QueryView::new(query);
    let mut scored: Vec<(RankScore, usize, &'a CommercialOffer)> = offers
        .iter()
        .enumerate()
        .map(|(index, offer)| (score_offer(&view, offer, ctx), index, offer))
        .collect();
    // The tie-break key is built ONCE per offer (`sort_by_cached_key`); the
    // pre-fix comparator rebuilt it inside EVERY comparison — four fresh
    // Strings per offered pair, O(n log n) full-title allocations for a
    // 10k-candidate batch. Order is byte-identical: total descending, then
    // the tie-break tuple ascending, then input index ascending.
    scored.sort_by_cached_key(|(score, index, offer)| {
        (std::cmp::Reverse(score.total), tie_break_key(offer), *index)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offer::{
        CommercialOffer, LifecycleStatus, ObservationOrigin, OfferProvenance, PriceVisibility,
        StockState, VariantAttribute, VariantOffer,
    };
    use crate::text::{SourceId, Text};

    fn source() -> SourceId {
        SourceId::new("test").unwrap()
    }

    fn attr(name: &str, value: &str) -> VariantAttribute {
        VariantAttribute {
            name: Text::new(name).unwrap(),
            value: Text::new(value).unwrap(),
        }
    }

    fn variant(index: usize, attrs: usize) -> VariantOffer {
        VariantOffer {
            variant_id: crate::text::VariantId::new(&format!("sku-{index}")).unwrap(),
            attributes: (0..attrs)
                .map(|a| attr(&format!("attr{a}"), &format!("val{index}-{a}")))
                .collect(),
            packaging: None,
            moq: None,
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            price_breaks: Vec::new(),
            lead_time: None,
        }
    }

    fn offer(index: usize, variants: usize, attrs: usize) -> CommercialOffer {
        CommercialOffer {
            source: source(),
            identity: crate::identity::ProductIdentity {
                manufacturer: Some(Text::new("ACME").unwrap()),
                manufacturer_part_number: Some(Text::new(&format!("PART{index}")).unwrap()),
                source_part_number: None,
                offer_id: None,
                canonical_url: None,
                category: None,
            },
            title: Text::new(&format!("ACME PART{index} res 10K case 0603")).unwrap(),
            description: None,
            currency: crate::money::Currency::CNY,
            price_breaks: vec![crate::offer::PriceBreak {
                min_quantity: crate::quantity::NonZeroQuantity::ONE,
                max_quantity: None,
                unit_price: crate::money::Money::parse(crate::money::Currency::CNY, "1.00")
                    .unwrap(),
                visibility: PriceVisibility::Public,
                account_scope: None,
                promotion: None,
            }],
            variants: (0..variants).map(|v| variant(v, attrs)).collect(),
            moq: None,
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            lead_time: None,
            packaging: Vec::new(),
            supplier: None,
            manufacturer: None,
            provenance: OfferProvenance {
                origin: ObservationOrigin::OfficialApi,
                source: source(),
                extractor_version: None,
                connector_version: None,
                normalization_version: None,
                content_digest: None,
                account_scope: None,
                locale: None,
                market: None,
                source_confidence_bp: None,
            },
            observed_at_ms: 1,
            price_visibility: PriceVisibility::Public,
            lifecycle: LifecycleStatus::Unknown,
        }
    }

    fn query() -> PartQuery {
        PartQuery {
            manufacturer: Some(Text::new("ACME").unwrap()),
            part_number: Text::new("PART7").unwrap(),
            package: Some(Text::new("0603").unwrap()),
            packaging: None,
            quantity: Some(crate::quantity::Quantity::new(100).unwrap()),
            attributes: vec![
                attr("attr1", "VAL3-1"),
                attr("attr2", "val3-2"),
                attr(" attr3 ", "Val3-3"),
                attr("attr4", "val3-4"),
            ],
        }
    }

    fn pricing_ctx<'a>(
        query: &'a PartQuery,
        ctx: &'a RankingContext,
    ) -> crate::quote::PricingContext<'a> {
        crate::quote::PricingContext {
            variant: if query.attributes.is_empty() {
                VariantRequest::None
            } else {
                VariantRequest::Attributes(&query.attributes)
            },
            packaging: ctx.packaging,
            account: ctx.account.as_ref(),
            now_ms: ctx.now_ms,
            apply_promotions: ctx.apply_promotions,
            shipping: None,
            tax: None,
            duty: None,
        }
    }

    #[test]
    fn resolved_scoped_quote_reports_the_same_scope_as_selected_scope() {
        for index in [0usize, 7, 42] {
            let offer = offer(index, 4, 8);
            let query = query();
            let ctx = RankingContext::at_quantity(crate::quantity::Quantity::new(100).unwrap());
            let pctx = pricing_ctx(&query, &ctx);
            let (resolution, variant, packaging) =
                crate::quote::resolve_quote_scoped(&offer, &pctx, ctx.quantity);
            if resolution.status != QuoteStatus::Resolved {
                continue;
            }
            let (expected_variant, expected_packaging) =
                crate::quote::selected_scope(&offer, &pctx);
            assert_eq!(
                variant.map(|value| value.variant_id.as_str()),
                expected_variant.map(|value| value.variant_id.as_str())
            );
            assert_eq!(
                packaging.map(|value| value.packaging),
                expected_packaging.map(|value| value.packaging)
            );
        }
    }

    #[test]
    fn ranking_pipeline_drops_the_per_candidate_query_and_quantity_work() {
        let offers: Vec<CommercialOffer> = (0..10_000).map(|i| offer(i, 4, 8)).collect();
        let query = query();
        let ctx = RankingContext::at_quantity(crate::quantity::Quantity::new(100).unwrap());
        let mut old_best = u64::MAX;
        let mut new_best = u64::MAX;
        for _ in 0..2 {
            let started = std::time::Instant::now();
            for offer in &offers {
                let pctx = pricing_ctx(&query, &ctx);
                // The pre-fix ranking path: a fresh QueryView + full match
                // (including a quantity quote), then its own scope/quote.
                let signals = crate::matching::match_offer(&query, offer, 0).signals;
                let _ = crate::quote::selected_scope(offer, &pctx);
                let _ = crate::quote::resolve_quote(offer, &pctx, ctx.quantity);
                std::hint::black_box(signals);
            }
            old_best = old_best.min(started.elapsed().as_nanos() as u64);

            let started = std::time::Instant::now();
            let view = QueryView::new(&query);
            for offer in &offers {
                let pctx = pricing_ctx(&query, &ctx);
                let signals = match_signals_in_view(&view, offer);
                let _ = crate::quote::resolve_quote_scoped(offer, &pctx, ctx.quantity);
                std::hint::black_box(signals);
            }
            new_best = new_best.min(started.elapsed().as_nanos() as u64);
        }
        println!(
            "PROBE rank_pipeline_10k old_ns={old_best} new_ns={new_best} \
             reduction_pct={}",
            (old_best.saturating_sub(new_best)) * 100 / old_best.max(1)
        );
    }

    #[test]
    fn ranking_builds_the_query_view_once_for_10k_candidates() {
        let offers: Vec<CommercialOffer> = (0..10_000).map(|i| offer(i, 4, 8)).collect();
        let query = query();
        let ctx = RankingContext::at_quantity(crate::quantity::Quantity::new(100).unwrap());
        crate::offer::normalization_work::reset();
        crate::matching::query_view_work::reset();
        let mut best = u64::MAX;
        let mut ranked = 0usize;
        for _ in 0..2 {
            let started = std::time::Instant::now();
            let out = rank_offers(&query, &offers, &ctx);
            best = best.min(started.elapsed().as_nanos() as u64);
            ranked = out.len();
        }
        let normalizations = crate::offer::normalization_work::snapshot();
        let query_views = crate::matching::query_view_work::snapshot();
        assert_eq!(ranked, 10_000);
        // The pre-fix path rebuilt the query view inside every candidate:
        // one view per rank call, never one per candidate.
        assert_eq!(query_views, 2, "one query view per rank call");
        // Candidate attributes are still normalized by matching + quote
        // resolution; the query side is not repeated per candidate.
        assert!(
            normalizations < 10_000 * 216 * 2,
            "query-side normalization must not repeat per candidate: {normalizations}"
        );
        println!(
            "PROBE rank_10k_offers best_ns={best} ranked={ranked} \
             normalizations={normalizations} query_views={query_views}"
        );
    }
}
