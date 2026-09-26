//! Offer assembly for 1688: reconcile the strategy drafts into one canonical
//! `CommercialOffer` (spec §7/§8).
//!
//! Reconciliation happens exactly once, here: each semantic field is
//! resolved by [`reconcile`] with the documented strategy authority. A
//! conflicting price is recorded and the strongest origin is selected —
//! never averaged, never guessed, and a variant-priced offer never exposes
//! its cheapest variant as the headline price.

use std::collections::{BTreeMap, BTreeSet};

use faktor_commerce::money::Currency;
use faktor_commerce::offer::{
    ObservationOrigin, OfferProvenance, PriceBreak, StockState, VariantOffer,
};
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{AccountScope, CanonicalUrl, Text};
use faktor_commerce::{
    CommercialOffer, LeadTime, PriceVisibility, ProductIdentity, SourceError, SourceId,
};

use super::extract::Draft;
use crate::context::{AccessVisibility, AcquireCtx};
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
    /// True when the page states inquiry/RFQ-only pricing.
    pub inquiry: bool,
    /// The strongest strategy that produced data.
    pub origin: ObservationOrigin,
}

/// Resolve one field across every draft.
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

/// Parse a lead time stated in days (`3-7天`, `7天`, `8 Weeks`).
pub fn parse_lead_time(raw: &str) -> Option<LeadTime> {
    if let Some(lead) = normalize::parse_lead_time_label(raw) {
        return Some(lead);
    }
    if !raw.contains('天') {
        return None;
    }
    let numbers: Vec<u32> = raw
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok())
        .collect();
    let first = *numbers.first()?;
    let second = numbers.get(1).copied().unwrap_or(first);
    let low = first.min(second);
    let high = first.max(second);
    LeadTime::new(u16::try_from(low).ok()?, u16::try_from(high).ok()?).ok()
}

fn currency_of(resolved: &ResolvedField) -> Currency {
    match text_of(resolved) {
        Some(raw) => match Currency::new(&raw.to_ascii_uppercase()) {
            Ok(currency) => currency,
            Err(_) => Currency::CNY,
        },
        None => Currency::CNY,
    }
}

/// Assemble the drafts into one offer. Fails typed when no usable identity
/// or price state exists.
pub fn assemble(
    source: &SourceId,
    ctx: &AcquireCtx,
    url: &CanonicalUrl,
    drafts: &[Draft],
    now_ms: u64,
    access: &AccessVisibility,
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

    let title = text_of(&title).ok_or(SourceError::ExtractionIncomplete)?;
    let title = Text::<512>::new(&title).map_err(|_| SourceError::ExtractionIncomplete)?;

    let variants = variants_of(drafts, &mut conflicts);
    let origin = strongest_origin(drafts);
    let currency = currency_of(&resolve(&observations, Field::Currency));

    let moq = quantity_of(&resolve(&observations, Field::MinimumOrder));
    let order_multiple = quantity_of(&resolve(&observations, Field::OrderMultiple));
    let stock = match resolve(&observations, Field::Stock) {
        ResolvedField::Value {
            value: FieldValue::Quantity(quantity),
            ..
        } => StockState::InStock { quantity },
        ResolvedField::Conflict { selected, .. } => match selected {
            FieldValue::Quantity(quantity) => StockState::InStock { quantity },
            FieldValue::Text(text) if text.as_str().contains("缺货") => StockState::OutOfStock,
            _ => StockState::Unknown,
        },
        _ => StockState::Unknown,
    };
    let lead_time =
        text_of(&resolve(&observations, Field::LeadTime)).and_then(|raw| parse_lead_time(&raw));
    let supplier_name = text_of(&resolve(&observations, Field::Supplier));
    let manufacturer_name = text_of(&resolve(&observations, Field::Manufacturer));

    // Page data decides whether a price exists (an inquiry has no number);
    // the capture authority — never the mere presence of a numeric price —
    // decides its visibility.
    let (price_visibility, price_account_scope) = if inquiry {
        (PriceVisibility::InquiryRequired, None)
    } else if price.value().is_some() || !variants.is_empty() {
        (access.price_visibility(), access.price_account_scope())
    } else {
        return Err(SourceError::ExtractionIncomplete);
    };

    let price_breaks = if inquiry || !variants.is_empty() {
        Vec::new()
    } else {
        offer_price_breaks(
            &price,
            price_visibility,
            price_account_scope.clone(),
            moq,
            now_ms,
        )?
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
                    min_quantity: variant
                        .moq
                        .or(moq)
                        .unwrap_or_else(|| NonZeroQuantity::new(1).expect("one")),
                    max_quantity: None,
                    unit_price,
                    visibility: price_visibility,
                    account_scope: price_account_scope.clone(),
                    promotion: None,
                }],
                _ => Vec::new(),
            },
            lead_time,
        })
        .collect();

    // The listing id comes from the payload when it states one; otherwise
    // the reference URL identifies the listing (`.../offer/<id>.html`).
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
            Text::<256>::new(&name)
                .ok()
                .map(|name| faktor_commerce::offer::Supplier {
                    id: None,
                    name,
                    country: None,
                    url: None,
                    account_scope: ctx.account_scope().cloned(),
                })
        }),
        manufacturer: manufacturer_name.and_then(|name| {
            Text::<256>::new(&name)
                .ok()
                .map(|name| faktor_commerce::offer::Manufacturer {
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

fn offer_price_breaks(
    price: &ResolvedField,
    visibility: PriceVisibility,
    account_scope: Option<AccountScope>,
    moq: Option<NonZeroQuantity>,
    _now_ms: u64,
) -> Result<Vec<PriceBreak>, SourceError> {
    let Some(unit_price) = money_of(price) else {
        return Ok(Vec::new());
    };
    Ok(vec![PriceBreak {
        min_quantity: moq.unwrap_or_else(|| NonZeroQuantity::new(1).expect("one")),
        max_quantity: None,
        unit_price,
        visibility,
        account_scope,
        promotion: None,
    }])
}

/// The variant matrix of the strongest draft that has one, with cross-draft
/// price disagreements recorded as conflicts.
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

/// The numeric listing id at the end of a detail URL.
fn listing_id_from_url(url: &CanonicalUrl) -> Option<String> {
    let path = url
        .as_str()
        .split(['?', '#'])
        .next()
        .unwrap_or(url.as_str());
    let last = path.rsplit('/').find(|segment| !segment.is_empty())?;
    let id = last.strip_suffix(".html").unwrap_or(last);
    if id.is_empty() || id.len() > 64 {
        return None;
    }
    Some(id.to_string())
}

/// Assemble a single draft, mapping a missing price state to `Ok(None)`
/// (discovery skips items it cannot price rather than fabricating one).
pub fn single_offer(
    source: &SourceId,
    ctx: &AcquireCtx,
    url: &CanonicalUrl,
    drafts: &[Draft],
    now_ms: u64,
    access: &AccessVisibility,
) -> Result<Option<CommercialOffer>, SourceError> {
    match assemble(source, ctx, url, drafts, now_ms, access) {
        Ok(assembly) => Ok(Some(assembly.offer)),
        Err(SourceError::ExtractionIncomplete) | Err(SourceError::ProductNotFound) => Ok(None),
        Err(other) => Err(other),
    }
}

/// The discovery shape of an assembled offer. A variant-priced offer never
/// advertises its cheapest variant as a headline price: the hint is dropped
/// when variant prices differ.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lead_times_parse_exactly() {
        assert_eq!(
            parse_lead_time("3-7天"),
            Some(LeadTime::new(3, 7).expect("lead"))
        );
        assert_eq!(
            parse_lead_time("7天"),
            Some(LeadTime::new(7, 7).expect("lead"))
        );
        assert_eq!(
            parse_lead_time("8 Weeks"),
            Some(LeadTime::new(56, 56).expect("lead"))
        );
        assert_eq!(parse_lead_time("soon"), None);
    }
}
