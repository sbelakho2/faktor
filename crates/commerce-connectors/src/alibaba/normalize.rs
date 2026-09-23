//! Offer assembly for Alibaba.com: reconcile the strategy drafts into one
//! canonical `CommercialOffer`. Same reconciliation vocabulary as every site
//! (documented authority ordering, conflicts recorded, never averaged), own
//! normalization logic.

use std::collections::{BTreeMap, BTreeSet};

use faktor_commerce::money::Currency;
use faktor_commerce::offer::{
    Manufacturer, ObservationOrigin, OfferProvenance, PriceBreak, StockState, Supplier,
    VariantOffer,
};
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{CommercialOffer, PriceVisibility, ProductIdentity, SourceError, SourceId};

use super::extract::Draft;
use crate::context::AcquireCtx;
use crate::contract::extract::{
    reconcile, Field, FieldObservation, FieldValue, ResolvedField, Strategy, VariantEntry,
};
use crate::contract::Discovery;
use crate::normalize;

/// The assembled result of one acquisition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembly {
    /// The canonical offer.
    pub offer: CommercialOffer,
    /// The variant matrix the offer prices are selected from.
    pub variants: Vec<VariantEntry>,
    /// Recorded `(field, authority, losing strategy)` conflicts.
    pub conflicts: Vec<(Field, Strategy, Strategy)>,
    /// True when the page states contact-supplier / RFQ-only pricing.
    pub inquiry: bool,
    /// The strongest strategy that produced data.
    pub origin: ObservationOrigin,
}

fn resolve(observations: &BTreeMap<Field, Vec<FieldObservation>>, field: Field) -> ResolvedField {
    match observations.get(&field) {
        Some(observations) => reconcile(observations),
        None => ResolvedField::Missing,
    }
}

fn text_of(resolved: &ResolvedField) -> Option<String> {
    resolved
        .value()
        .and_then(FieldValue::as_text)
        .map(str::to_string)
}

fn money_of(resolved: &ResolvedField) -> Option<faktor_commerce::Money> {
    resolved.value().and_then(FieldValue::as_money)
}

fn quantity_of(resolved: &ResolvedField) -> Option<NonZeroQuantity> {
    resolved.value().and_then(FieldValue::as_quantity)
}

fn currency_of(resolved: &ResolvedField) -> Option<Currency> {
    text_of(resolved).and_then(|raw| {
        normalize::currency_from_symbol(&raw)
            .or_else(|| Currency::new(&raw.to_ascii_uppercase()).ok())
    })
}

/// Resolve the offer currency from the declared `Currency` field and every
/// monetary observation:
///
/// * a declared currency must agree with every observed price, or the
///   acquisition is refused typed (never a silent conversion),
/// * without a declaration, the unique currency of the observed prices
///   decides,
/// * only an inquiry with no monetary observation at all falls back to the
///   documented buyer-surface default (the currency is unused there).
fn resolve_currency(
    declared: Option<Currency>,
    price: &ResolvedField,
    variants: &[VariantEntry],
    drafts: &[Draft],
) -> Result<Currency, SourceError> {
    let mut observed: Vec<Currency> = Vec::new();
    if let Some(money) = money_of(price) {
        observed.push(money.currency);
    }
    for variant in variants {
        if let Some(money) = variant.unit_price {
            observed.push(money.currency);
        }
    }
    for draft in drafts {
        for (_, money) in &draft.tiers {
            observed.push(money.currency);
        }
    }
    match declared {
        Some(declared) => {
            if observed.iter().any(|currency| *currency != declared) {
                return Err(SourceError::ExtractionConflict);
            }
            Ok(declared)
        }
        None => {
            let mut unique = observed.into_iter();
            let first = unique.next();
            if unique.any(|currency| Some(currency) != first) {
                return Err(SourceError::ExtractionConflict);
            }
            Ok(first.unwrap_or(super::DEFAULT_CURRENCY))
        }
    }
}

/// Assemble the drafts into one offer.
pub fn assemble(
    source: &SourceId,
    ctx: &AcquireCtx,
    url: &CanonicalUrl,
    drafts: &[Draft],
    now_ms: u64,
) -> Result<Assembly, SourceError> {
    let mut observations: BTreeMap<Field, Vec<FieldObservation>> = BTreeMap::new();
    for draft in drafts {
        for observation in &draft.observations {
            observations
                .entry(observation.field)
                .or_default()
                .push(observation.clone());
        }
    }
    let mut conflicts: Vec<(Field, Strategy, Strategy)> = Vec::new();
    for (field, field_observations) in &observations {
        if let ResolvedField::Conflict {
            observations: recorded,
            authority,
            ..
        } = reconcile(field_observations)
        {
            if let Some(loser) = recorded
                .iter()
                .find(|observation| observation.strategy != authority)
            {
                conflicts.push((*field, authority, loser.strategy));
            }
        }
    }

    let title = resolve(&observations, Field::Title);
    let offer_id = resolve(&observations, Field::OfferId);
    let price = resolve(&observations, Field::Price);
    let inquiry = text_of(&resolve(&observations, Field::InquiryOnly)).is_some();
    let declared_currency = currency_of(&resolve(&observations, Field::Currency));

    let title = text_of(&title).ok_or(SourceError::ExtractionIncomplete)?;
    let title = Text::<512>::new(&title).map_err(|_| SourceError::ExtractionIncomplete)?;

    let variants = variants_of(drafts, &mut conflicts);
    let currency = resolve_currency(declared_currency, &price, &variants, drafts)?;
    let origin = strongest_origin(drafts);

    let moq = quantity_of(&resolve(&observations, Field::MinimumOrder));
    let order_multiple = quantity_of(&resolve(&observations, Field::OrderMultiple));
    let stock = match resolve(&observations, Field::Stock) {
        ResolvedField::Value {
            value: FieldValue::Quantity(quantity),
            ..
        } => StockState::InStock { quantity },
        ResolvedField::Conflict {
            selected: FieldValue::Quantity(quantity),
            ..
        } => StockState::InStock { quantity },
        ResolvedField::Conflict { .. } => StockState::Unknown,
        _ => StockState::Unknown,
    };
    let lead_time = text_of(&resolve(&observations, Field::LeadTime))
        .and_then(|raw| normalize::parse_lead_time_label(&raw));
    let supplier_name = text_of(&resolve(&observations, Field::Supplier));
    let manufacturer_name = text_of(&resolve(&observations, Field::Manufacturer));

    let price_visibility = if inquiry {
        PriceVisibility::InquiryRequired
    } else if price.value().is_some() || !variants.is_empty() || drafts_have_tiers(drafts) {
        PriceVisibility::Public
    } else {
        return Err(SourceError::ExtractionIncomplete);
    };

    let price_breaks = if inquiry || !variants.is_empty() {
        Vec::new()
    } else if drafts_have_tiers(drafts) {
        tiers_of(drafts, currency, price_visibility)?
    } else {
        match money_of(&price) {
            Some(unit_price) => vec![PriceBreak {
                min_quantity: moq.unwrap_or_else(one),
                max_quantity: None,
                unit_price,
                visibility: price_visibility,
                account_scope: None,
                promotion: None,
            }],
            None => Vec::new(),
        }
    };

    let variant_offers: Vec<VariantOffer> = variants
        .iter()
        .map(|variant| VariantOffer {
            variant_id: variant.variant_id.clone(),
            attributes: variant.attributes.clone(),
            packaging: None,
            moq: variant.moq.or(moq),
            order_multiple,
            standard_pack: None,
            stock: variant.stock,
            price_breaks: match variant.unit_price {
                Some(unit_price) if !inquiry => vec![PriceBreak {
                    min_quantity: variant.moq.or(moq).unwrap_or_else(one),
                    max_quantity: None,
                    unit_price,
                    visibility: PriceVisibility::Public,
                    account_scope: None,
                    promotion: None,
                }],
                _ => Vec::new(),
            },
            lead_time,
        })
        .collect();

    let listing_id: Option<String> = offer_id
        .value()
        .and_then(FieldValue::as_text)
        .map(str::to_string)
        .or_else(|| listing_id_from_url(url));
    let identity = ProductIdentity {
        manufacturer: manufacturer_name
            .as_deref()
            .and_then(|name| Text::<128>::new(name).ok()),
        manufacturer_part_number: None,
        source_part_number: listing_id
            .as_deref()
            .and_then(|raw| Text::<256>::new(raw).ok()),
        offer_id: listing_id
            .as_deref()
            .and_then(|raw| Text::<512>::new(raw).ok()),
        canonical_url: Some(url.clone()),
        category: None,
    };

    let offer = CommercialOffer {
        source: source.clone(),
        identity,
        title,
        description: None,
        currency,
        price_breaks,
        variants: variant_offers,
        moq,
        order_multiple,
        standard_pack: None,
        stock,
        lead_time,
        packaging: Vec::new(),
        supplier: supplier_name.and_then(|name| {
            Text::<256>::new(&name).ok().map(|name| Supplier {
                id: None,
                name,
                country: None,
                url: None,
                account_scope: ctx.account_scope().cloned(),
            })
        }),
        manufacturer: manufacturer_name.and_then(|name| {
            Text::<256>::new(&name).ok().map(|name| Manufacturer {
                id: None,
                name,
                country: None,
                url: None,
            })
        }),
        provenance: OfferProvenance {
            origin,
            source: source.clone(),
            extractor_version: None,
            connector_version: Text::<64>::new(normalize::CONNECTOR_VERSION).ok(),
            normalization_version: Text::<64>::new(normalize::NORMALIZATION_VERSION).ok(),
            content_digest: None,
            account_scope: ctx.account_scope().cloned(),
            locale: ctx.locale().cloned(),
            market: ctx.market().cloned(),
            source_confidence_bp: None,
        },
        observed_at_ms: now_ms,
        price_visibility,
        lifecycle: faktor_commerce::LifecycleStatus::Unknown,
    };
    offer.validate().map_err(normalize::map_offer_error)?;

    Ok(Assembly {
        offer,
        variants,
        conflicts,
        inquiry,
        origin,
    })
}

fn one() -> NonZeroQuantity {
    NonZeroQuantity::new(1).expect("one")
}

fn drafts_have_tiers(drafts: &[Draft]) -> bool {
    drafts.iter().any(|draft| !draft.tiers.is_empty())
}

fn tiers_of(
    drafts: &[Draft],
    currency: Currency,
    visibility: PriceVisibility,
) -> Result<Vec<PriceBreak>, SourceError> {
    let mut ordered: Vec<&Draft> = drafts
        .iter()
        .filter(|draft| !draft.tiers.is_empty())
        .collect();
    ordered.sort_by_key(|draft| std::cmp::Reverse(draft.strategy.authority_rank()));
    let Some(primary) = ordered.first() else {
        return Ok(Vec::new());
    };
    let mut tiers: Vec<(NonZeroQuantity, faktor_commerce::Money)> = primary.tiers.clone();
    tiers.sort_by_key(|(quantity, _)| quantity.get());
    let mut breaks: Vec<PriceBreak> = Vec::new();
    for (index, (min_quantity, unit_price)) in tiers.iter().enumerate() {
        if unit_price.currency != currency {
            continue;
        }
        breaks.push(PriceBreak {
            min_quantity: *min_quantity,
            max_quantity: tiers
                .get(index + 1)
                .and_then(|(next, _)| NonZeroQuantity::new(next.get().saturating_sub(1)).ok()),
            unit_price: *unit_price,
            visibility,
            account_scope: None,
            promotion: None,
        });
    }
    if breaks.is_empty() {
        return Ok(Vec::new());
    }
    crate::normalize::validate_tiers(&breaks, currency)
        .map_err(|_| SourceError::ExtractionConflict)?;
    Ok(breaks)
}

fn variants_of(
    drafts: &[Draft],
    conflicts: &mut Vec<(Field, Strategy, Strategy)>,
) -> Vec<VariantEntry> {
    let mut ordered: Vec<&Draft> = drafts
        .iter()
        .filter(|draft| !draft.variants.is_empty())
        .collect();
    ordered.sort_by_key(|draft| std::cmp::Reverse(draft.strategy.authority_rank()));
    let Some(primary) = ordered.first() else {
        return Vec::new();
    };
    for other in ordered.iter().skip(1) {
        for variant in &primary.variants {
            if let Some(competing) = other
                .variants
                .iter()
                .find(|candidate| candidate.variant_id == variant.variant_id)
            {
                if competing.unit_price != variant.unit_price
                    && !conflicts.contains(&(Field::Variant, primary.strategy, other.strategy))
                {
                    conflicts.push((Field::Variant, primary.strategy, other.strategy));
                }
            }
        }
    }
    primary.variants.clone()
}

fn strongest_origin(drafts: &[Draft]) -> ObservationOrigin {
    Strategy::ORDER
        .into_iter()
        .find(|strategy| {
            drafts.iter().any(|draft| {
                draft.strategy == *strategy
                    && draft.outcome == crate::contract::extract::StrategyOutcome::Success
            })
        })
        .map(Strategy::origin)
        .unwrap_or(ObservationOrigin::RenderedText)
}

/// The discovery shape of an assembled offer (a variant-priced offer never
/// advertises its cheapest variant).
pub fn discovery(assembly: &Assembly) -> Discovery {
    let mut discovery = normalize::discovery_from_offer(&assembly.offer, None);
    if assembly.inquiry {
        discovery.price_hint = None;
        discovery.price_visibility = PriceVisibility::InquiryRequired;
    }
    let distinct: BTreeSet<i64> = assembly
        .variants
        .iter()
        .filter_map(|variant| variant.unit_price.map(|money| money.micros))
        .collect();
    if distinct.len() > 1 {
        discovery.price_hint = None;
    }
    discovery
}

fn listing_id_from_url(url: &CanonicalUrl) -> Option<String> {
    let path = url
        .as_str()
        .split(['?', '#'])
        .next()
        .unwrap_or(url.as_str());
    let last = path.rsplit('/').find(|segment| !segment.is_empty())?;
    let id = last.strip_suffix(".html").unwrap_or(last);
    let digits: String = id
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.len() < 6 {
        return None;
    }
    Some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_ids_come_from_detail_urls() {
        let url = CanonicalUrl::parse(
            "https://www.alibaba.com/product-detail/USB-Cable_1600123456789.html",
        )
        .expect("url");
        assert_eq!(listing_id_from_url(&url), Some("1600123456789".to_string()));
    }
}
