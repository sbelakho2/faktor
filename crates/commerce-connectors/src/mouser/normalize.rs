//! Mouser Search API JSON → `faktor-commerce` normalization.
//!
//! Endpoints covered (sanitized fixtures under `fixtures/mouser/`):
//!
//! * `POST /api/v1/search/partnumber` — exact part-number search;
//! * `POST /api/v1/search/keyword` — keyword discovery.
//!
//! Rules, shared with every site module:
//!
//! * price tokens are captured as [`RawValue`] and parsed by
//!   [`crate::normalize::money_from_raw`] with integer arithmetic — no
//!   floating point ever touches a price;
//! * more than [`MAX_PRICE_BREAKS`] documented breaks is schema drift: a
//!   typed error, never a silent truncation;
//! * identity text (MPN, source part number) is strict — a bidi-overridden
//!   MPN is refused, never sanitized into a spoof; display text is
//!   sanitized and bounded.

use faktor_commerce::money::Currency;
use faktor_commerce::offer::{
    CommercialOffer, Manufacturer, PriceBreak, PriceVisibility, VariantOffer,
};
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{LifecycleStatus, ProductIdentity, SourceError, SourceId, StockState};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::browser::FallbackObservation;
use crate::context::AcquireCtx;
use crate::normalize::{self, NormalizeError};

/// The documented maximum price breaks in a Mouser Search API part.
pub const MAX_PRICE_BREAKS: usize = 4;

/// The documented default currency of the Mouser US Search API. Only used
/// when an offer carries no price at all, where the currency is unused.
pub(crate) const DEFAULT_CURRENCY: Currency = Currency::USD;

/// One Mouser API error entry.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ApiError {
    #[serde(rename = "Code")]
    pub(crate) code: Option<String>,
    #[serde(rename = "Message")]
    pub(crate) message: Option<String>,
    #[serde(rename = "PropertyName")]
    pub(crate) property_name: Option<String>,
}

/// One price break as the API reports it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PriceBreakRaw {
    #[serde(rename = "Quantity")]
    pub(crate) quantity: Option<Box<RawValue>>,
    #[serde(rename = "Price")]
    pub(crate) price: Option<Box<RawValue>>,
    #[serde(rename = "Currency")]
    pub(crate) currency: Option<String>,
}

/// One part as the API reports it. Every numeric token stays raw so no
/// float can be introduced by deserialization.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Part {
    #[serde(rename = "MouserPartNumber")]
    pub(crate) mouser_part_number: Option<String>,
    #[serde(rename = "ManufacturerPartNumber")]
    pub(crate) manufacturer_part_number: Option<String>,
    #[serde(rename = "Manufacturer")]
    pub(crate) manufacturer: Option<String>,
    #[serde(rename = "Description")]
    pub(crate) description: Option<String>,
    #[serde(rename = "Availability")]
    pub(crate) availability: Option<String>,
    #[serde(rename = "AvailabilityInStock")]
    pub(crate) availability_in_stock: Option<Box<RawValue>>,
    #[serde(rename = "LeadTime")]
    pub(crate) lead_time: Option<String>,
    #[serde(rename = "Min")]
    pub(crate) min: Option<Box<RawValue>>,
    #[serde(rename = "Mult")]
    pub(crate) mult: Option<Box<RawValue>>,
    #[serde(rename = "StandardPackQuantity")]
    pub(crate) standard_pack_quantity: Option<Box<RawValue>>,
    #[serde(rename = "LifecycleStatus")]
    pub(crate) lifecycle_status: Option<String>,
    #[serde(rename = "ProductDetailUrl")]
    pub(crate) product_detail_url: Option<String>,
    #[serde(rename = "PriceBreaks")]
    pub(crate) price_breaks: Vec<PriceBreakRaw>,
}

/// The `SearchResults` object.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct SearchResults {
    #[serde(rename = "NumberOfResult")]
    pub(crate) number_of_result: Option<Box<RawValue>>,
    #[serde(rename = "Parts")]
    pub(crate) parts: Vec<Part>,
}

/// One whole Search API response.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct SearchResponse {
    #[serde(rename = "Errors")]
    pub(crate) errors: Vec<ApiError>,
    #[serde(rename = "SearchResults")]
    pub(crate) search_results: SearchResults,
}

/// Parse a response body. Malformed JSON, wrong types and over-deep nesting
/// are a typed [`SourceError`], never a panic.
pub(crate) fn parse_search_response(body: &[u8]) -> Result<SearchResponse, SourceError> {
    if !normalize::json_nesting_within(body, normalize::MAX_JSON_NESTING) {
        return Err(SourceError::ExtractionIncomplete);
    }
    serde_json::from_slice::<SearchResponse>(body).map_err(|_| SourceError::ExtractionIncomplete)
}

/// Normalize every part of a discovery response, bounded by `limit`. A part
/// that cannot be normalized fails the whole call (schema drift is never
/// silently skipped).
pub(crate) fn offers_from_response(
    response: &SearchResponse,
    source: &SourceId,
    ctx: &AcquireCtx,
    limit: u16,
) -> Result<Vec<CommercialOffer>, SourceError> {
    let mut offers = Vec::new();
    for part in response
        .search_results
        .parts
        .iter()
        .take(usize::from(limit))
    {
        offers.push(normalize_part(part, source, ctx)?);
    }
    Ok(offers)
}

/// Normalize the first part of an exact-search response.
pub(crate) fn first_offer(
    response: &SearchResponse,
    source: &SourceId,
    ctx: &AcquireCtx,
) -> Result<CommercialOffer, SourceError> {
    match response.search_results.parts.first() {
        Some(part) => normalize_part(part, source, ctx),
        None => Err(SourceError::ProductNotFound),
    }
}

/// Normalize one part into the canonical offer.
pub(crate) fn normalize_part(
    part: &Part,
    source: &SourceId,
    ctx: &AcquireCtx,
) -> Result<CommercialOffer, SourceError> {
    let mut identity = ProductIdentity::empty();
    identity.offer_id = part
        .mouser_part_number
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| Text::<512>::new(value).ok());
    identity.manufacturer_part_number = match part.manufacturer_part_number.as_deref() {
        Some(mpn) if !mpn.trim().is_empty() => Some(normalize::strict_text::<256>(mpn)?),
        _ => None,
    };
    identity.source_part_number = match part.mouser_part_number.as_deref() {
        Some(sku) if !sku.trim().is_empty() => Some(normalize::strict_text::<256>(sku)?),
        _ => None,
    };
    identity.canonical_url = part
        .product_detail_url
        .as_deref()
        .and_then(|url| CanonicalUrl::parse(url).ok());
    if identity.is_empty() {
        return Err(NormalizeError::MissingIdentity.into());
    }

    let fallback_title = identity
        .manufacturer_part_number
        .as_ref()
        .map(Text::as_str)
        .or_else(|| identity.source_part_number.as_ref().map(Text::as_str))
        .unwrap_or("part");
    let title = normalize::display_text::<512>(
        part.description.as_deref().unwrap_or(fallback_title),
        fallback_title,
    )?;
    let description = match part.description.as_deref() {
        Some(description) if !description.trim().is_empty() => Some(
            normalize::display_text::<4096>(description, title.as_str())?,
        ),
        _ => None,
    };

    let (currency, price_breaks) = normalize_price_breaks(part)?;
    let moq = quantity_token(part.min.as_deref())?;
    let order_multiple = quantity_token(part.mult.as_deref())?;
    let standard_pack = quantity_token(part.standard_pack_quantity.as_deref())?;
    let stock = normalize_stock(part);
    let lead_time = part
        .lead_time
        .as_deref()
        .and_then(normalize::parse_lead_time_label);
    let manufacturer = part
        .manufacturer
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(|name| {
            normalize::display_text::<256>(name, "unknown").map(|name| Manufacturer {
                id: None,
                name,
                country: None,
                url: None,
            })
        })
        .transpose()?;

    let price_visibility = if price_breaks.is_empty() {
        PriceVisibility::Unknown
    } else {
        PriceVisibility::Public
    };
    let offer = CommercialOffer {
        source: source.clone(),
        identity,
        title,
        description,
        currency: currency.unwrap_or(DEFAULT_CURRENCY),
        price_breaks,
        variants: Vec::<VariantOffer>::new(),
        moq,
        order_multiple,
        standard_pack,
        stock,
        lead_time,
        packaging: Vec::new(),
        supplier: None,
        manufacturer,
        provenance: normalize::provenance(
            source,
            ctx,
            faktor_commerce::offer::ObservationOrigin::OfficialApi,
        ),
        observed_at_ms: ctx.now_ms(),
        price_visibility,
        lifecycle: normalize_lifecycle(part.lifecycle_status.as_deref()),
    };
    offer
        .validate()
        .map_err(|error| normalize::map_offer_error(error).to_source_error())?;
    Ok(offer)
}

/// Build an offer from a bounded browser observation (API outage / URL-only
/// reference). Prices are never taken from the browser: the offer carries no
/// price and `price_visibility: unknown`.
pub(crate) fn offer_from_observation(
    observation: &FallbackObservation,
    url: &CanonicalUrl,
    source: &SourceId,
    ctx: &AcquireCtx,
) -> Result<CommercialOffer, SourceError> {
    let mpn = observation
        .get("mpn")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(SourceError::ExtractionIncomplete)?;
    let mut identity = ProductIdentity::empty();
    identity.manufacturer_part_number = Some(normalize::strict_text::<256>(mpn)?);
    identity.canonical_url = Some(url.clone());
    identity.manufacturer = observation
        .get("manufacturer")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| normalize::display_text::<128>(value, "unknown"))
        .transpose()?;
    let title = normalize::display_text::<512>(observation.get("description").unwrap_or(mpn), mpn)?;
    let stock = observation
        .get("availability")
        .map(normalize::parse_stock_label)
        .unwrap_or(StockState::Unknown);
    let lead_time = observation
        .get("lead_time")
        .and_then(normalize::parse_lead_time_label);
    let offer = CommercialOffer {
        source: source.clone(),
        identity,
        title,
        description: None,
        currency: DEFAULT_CURRENCY,
        price_breaks: Vec::new(),
        variants: Vec::new(),
        moq: None,
        order_multiple: None,
        standard_pack: None,
        stock,
        lead_time,
        packaging: Vec::new(),
        supplier: None,
        manufacturer: None,
        provenance: normalize::provenance(
            source,
            ctx,
            faktor_commerce::offer::ObservationOrigin::BrowserNetwork,
        ),
        observed_at_ms: ctx.now_ms(),
        price_visibility: PriceVisibility::Unknown,
        lifecycle: LifecycleStatus::Unknown,
    };
    offer
        .validate()
        .map_err(|error| normalize::map_offer_error(error).to_source_error())?;
    Ok(offer)
}

fn normalize_price_breaks(
    part: &Part,
) -> Result<(Option<Currency>, Vec<PriceBreak>), NormalizeError> {
    if part.price_breaks.is_empty() {
        return Ok((None, Vec::new()));
    }
    if part.price_breaks.len() > MAX_PRICE_BREAKS {
        return Err(NormalizeError::Schema);
    }
    let mut currency: Option<Currency> = None;
    let mut breaks = Vec::new();
    for raw in &part.price_breaks {
        let quantity = raw
            .quantity
            .as_deref()
            .ok_or(NormalizeError::InvalidQuantity)?;
        let min_quantity = normalize::nonzero_quantity(normalize::parse_u64_raw(quantity)?)?
            .ok_or(NormalizeError::InvalidQuantity)?;
        let price = raw.price.as_deref().ok_or(NormalizeError::InvalidMoney)?;
        let label = raw
            .currency
            .as_deref()
            .ok_or(NormalizeError::CurrencyConflict)?;
        let break_currency =
            normalize::currency_from_symbol(label).ok_or(NormalizeError::CurrencyConflict)?;
        match currency {
            Some(existing) if existing != break_currency => {
                return Err(NormalizeError::CurrencyConflict)
            }
            None => currency = Some(break_currency),
            _ => {}
        }
        let unit_price = normalize::money_from_raw(price, break_currency)?;
        breaks.push(PriceBreak {
            min_quantity,
            max_quantity: None,
            unit_price,
            visibility: PriceVisibility::Public,
            account_scope: None,
            promotion: None,
        });
    }
    normalize::validate_tiers(&breaks, currency.unwrap_or(DEFAULT_CURRENCY))?;
    Ok((currency, breaks))
}

fn quantity_token(raw: Option<&RawValue>) -> Result<Option<NonZeroQuantity>, NormalizeError> {
    match raw {
        Some(raw) => normalize::parse_nonzero_quantity_raw(raw),
        None => Ok(None),
    }
}

fn normalize_stock(part: &Part) -> StockState {
    if let Some(raw) = part.availability_in_stock.as_deref() {
        if let Ok(text) = normalize::raw_token_text(raw) {
            match normalize::parse_grouped_u64(&text) {
                Some(0) => return StockState::OutOfStock,
                Some(value) => {
                    if let Ok(Some(quantity)) = normalize::nonzero_quantity(value) {
                        return StockState::InStock { quantity };
                    }
                }
                None => {}
            }
        }
    }
    part.availability
        .as_deref()
        .map(normalize::parse_stock_label)
        .unwrap_or(StockState::Unknown)
}

fn normalize_lifecycle(raw: Option<&str>) -> LifecycleStatus {
    raw.map(normalize::lifecycle_from_label)
        .unwrap_or(LifecycleStatus::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport;
    use faktor_commerce::money::Money;

    fn source() -> SourceId {
        SourceId::new("mouser").expect("valid source")
    }

    fn ctx() -> crate::context::AcquireCtx {
        testsupport::ctx()
    }

    fn offer() -> CommercialOffer {
        let body = crate::testing::fixture("mouser/partnumber_search.json").expect("fixture");
        let response = parse_search_response(body.as_bytes()).expect("parse");
        first_offer(&response, &source(), &ctx()).expect("normalize")
    }

    #[test]
    fn mpn_manufacturer_stock_and_terms_are_normalized() {
        let offer = offer();
        assert_eq!(
            offer
                .identity
                .manufacturer_part_number
                .as_ref()
                .map(Text::as_str),
            Some("TPS5430DDAR")
        );
        assert_eq!(
            offer.manufacturer.as_ref().map(|m| m.name.as_str()),
            Some("Texas Instruments")
        );
        assert_eq!(offer.stock.available_quantity(), Some(1234));
        assert_eq!(offer.moq.map(NonZeroQuantity::get), Some(1));
        assert_eq!(offer.order_multiple.map(NonZeroQuantity::get), Some(1));
        assert_eq!(offer.standard_pack.map(NonZeroQuantity::get), Some(2500));
        assert_eq!(
            offer.lead_time,
            Some(faktor_commerce::LeadTime::new(56, 56).expect("lead"))
        );
        assert_eq!(offer.lifecycle, LifecycleStatus::Active);
        assert_eq!(offer.price_visibility, PriceVisibility::Public);
        assert_eq!(offer.price_breaks.len(), MAX_PRICE_BREAKS);
        assert_eq!(
            offer.price_breaks[0].unit_price,
            Money::from_micros(Currency::USD, 4_920_000)
        );
        assert_eq!(
            offer.price_breaks[3].unit_price,
            Money::from_micros(Currency::USD, 2_980_000)
        );
        assert_eq!(offer.price_breaks[3].min_quantity.get(), 1000);
    }

    #[test]
    fn no_price_part_is_unknown_visibility_not_fabricated() {
        let body = crate::testing::fixture("mouser/keyword_search.json").expect("fixture");
        let response = parse_search_response(body.as_bytes()).expect("parse");
        let offers = offers_from_response(&response, &source(), &ctx(), 50).expect("normalize");
        assert_eq!(offers.len(), 2);
        let second = &offers[1];
        assert!(second.price_breaks.is_empty());
        assert_eq!(second.price_visibility, PriceVisibility::Unknown);
        assert_eq!(second.stock, StockState::OutOfStock);
        assert_eq!(second.cheapest_unit_price(), None);
    }

    #[test]
    fn bounded_by_requested_limit() {
        let body = crate::testing::fixture("mouser/keyword_search.json").expect("fixture");
        let response = parse_search_response(body.as_bytes()).expect("parse");
        let offers = offers_from_response(&response, &source(), &ctx(), 1).expect("normalize");
        assert_eq!(offers.len(), 1);
    }

    #[test]
    fn more_than_four_breaks_is_typed_schema_drift() {
        let body = crate::testing::fixture("mouser/huge_price_breaks.json").expect("fixture");
        let response = parse_search_response(body.as_bytes()).expect("parse");
        assert_eq!(
            first_offer(&response, &source(), &ctx()),
            Err(SourceError::ExtractionIncomplete)
        );
    }

    #[test]
    fn mixed_currencies_are_a_typed_conflict() {
        let body = crate::testing::fixture("mouser/mixed_currencies.json").expect("fixture");
        let response = parse_search_response(body.as_bytes()).expect("parse");
        assert_eq!(
            first_offer(&response, &source(), &ctx()),
            Err(SourceError::ExtractionConflict)
        );
    }

    #[test]
    fn hostile_tokens_never_become_a_price() {
        let body = crate::testing::fixture("mouser/wrong_types.json").expect("fixture");
        let response = parse_search_response(body.as_bytes()).expect("parse");
        let error = first_offer(&response, &source(), &ctx()).expect_err("hostile token");
        assert!(matches!(
            error,
            SourceError::ExtractionIncomplete | SourceError::ExtractionConflict
        ));
    }

    #[test]
    fn bidi_mpn_is_refused_not_sanitized() {
        let body = crate::testing::fixture("mouser/unicode_hostile.json").expect("fixture");
        let response = parse_search_response(body.as_bytes()).expect("parse");
        assert_eq!(
            first_offer(&response, &source(), &ctx()),
            Err(SourceError::ExtractionIncomplete)
        );
    }

    #[test]
    fn malformed_and_deeply_nested_bodies_are_typed() {
        let malformed = crate::testing::fixture("mouser/malformed.json").expect("fixture");
        assert!(matches!(
            parse_search_response(malformed.as_bytes()),
            Err(SourceError::ExtractionIncomplete)
        ));
        let deep = crate::testing::fixture("mouser/deep_nesting.json").expect("fixture");
        assert!(matches!(
            parse_search_response(deep.as_bytes()),
            Err(SourceError::ExtractionIncomplete)
        ));
    }

    #[test]
    fn huge_arrays_are_bounded_by_the_requested_limit() {
        let parts: Vec<serde_json::Value> = (0..2048)
            .map(|index| {
                serde_json::json!({
                    "MouserPartNumber": format!("595-PART{index:06}"),
                    "ManufacturerPartNumber": format!("PART{index:06}"),
                    "Manufacturer": "Sanitized",
                    "Description": "Buck regulator",
                    "Availability": "1 In Stock",
                    "Min": "1",
                    "Mult": "1",
                    "PriceBreaks": [
                        { "Quantity": 1, "Price": "1.00", "Currency": "USD" }
                    ]
                })
            })
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({
            "Errors": [],
            "SearchResults": { "NumberOfResult": 2048, "Parts": parts }
        }))
        .expect("serialize");
        let response = parse_search_response(&body).expect("parse");
        let offers = offers_from_response(&response, &source(), &ctx(), 3).expect("normalize");
        assert_eq!(offers.len(), 3);
    }

    #[test]
    fn missing_parts_is_product_not_found() {
        let response =
            parse_search_response(br#"{"Errors":[],"SearchResults":{"Parts":[]}}"#).expect("parse");
        assert_eq!(
            first_offer(&response, &source(), &ctx()),
            Err(SourceError::ProductNotFound)
        );
    }
}
