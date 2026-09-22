//! LCSC first-party JSON API → `faktor-commerce` normalization.
//!
//! Endpoints covered (sanitized fixtures under `fixtures/lcsc/`):
//!
//! * `GET /wmsc/search/global` — KeywordSearch discovery;
//! * `GET /wmsc/product/detail` — ProductDetails.
//!
//! LCSC returns **regular** and **discounted** price lists. When a discount
//! list is present it is the applicable price list and the offer is marked
//! [`PriceVisibility::Promotional`]; a malformed discount list is a typed
//! error, never silently replaced by the regular list.
//!
//! Prices are captured as [`RawValue`] and parsed with integer arithmetic;
//! the documented `ladder` is the inclusive minimum quantity.

use faktor_commerce::money::Currency;
use faktor_commerce::offer::{
    CommercialOffer, Manufacturer, PackagingOption, PriceBreak, PriceVisibility,
};
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{ProductIdentity, SourceError, SourceId, StockState};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::context::AcquireCtx;
use crate::normalize::{self, NormalizeError};

/// The documented maximum price tiers accepted from one LCSC price list.
pub const MAX_PRICE_BREAKS: usize = 32;

/// One LCSC price entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PriceRaw {
    #[serde(rename = "ladder")]
    pub(crate) ladder: Option<Box<RawValue>>,
    #[serde(rename = "productPrice")]
    pub(crate) product_price: Option<Box<RawValue>>,
    #[serde(rename = "currencySymbol")]
    pub(crate) currency_symbol: Option<String>,
    #[serde(rename = "currencyCode")]
    pub(crate) currency_code: Option<String>,
}

/// One LCSC product record.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Product {
    #[serde(rename = "productCode")]
    pub(crate) product_code: Option<String>,
    #[serde(rename = "productModel")]
    pub(crate) product_model: Option<String>,
    #[serde(rename = "brandNameEn")]
    pub(crate) brand_name_en: Option<String>,
    #[serde(rename = "productDescEn")]
    pub(crate) product_desc_en: Option<String>,
    #[serde(rename = "stockNumber")]
    pub(crate) stock_number: Option<Box<RawValue>>,
    #[serde(rename = "minBuyNumber")]
    pub(crate) min_buy_number: Option<Box<RawValue>>,
    #[serde(rename = "standardPack")]
    pub(crate) standard_pack: Option<Box<RawValue>>,
    #[serde(rename = "packingType")]
    pub(crate) packing_type: Option<String>,
    #[serde(rename = "leadTime")]
    pub(crate) lead_time: Option<String>,
    #[serde(rename = "productStatus")]
    pub(crate) product_status: Option<String>,
    #[serde(rename = "productUrl")]
    pub(crate) product_url: Option<String>,
    #[serde(rename = "currencySymbol")]
    pub(crate) currency_symbol: Option<String>,
    #[serde(rename = "currencyCode")]
    pub(crate) currency_code: Option<String>,
    #[serde(rename = "productPriceList")]
    pub(crate) product_price_list: Vec<PriceRaw>,
    #[serde(rename = "productDiscountPriceList")]
    pub(crate) product_discount_price_list: Vec<PriceRaw>,
}

/// The `result` object of a search response.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct SearchResult {
    #[serde(rename = "totalCount")]
    pub(crate) total_count: Option<Box<RawValue>>,
    #[serde(rename = "productList")]
    pub(crate) product_list: Vec<Product>,
}

/// One whole LCSC response.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Response {
    pub(crate) code: Option<Box<RawValue>>,
    pub(crate) result: Option<SearchResult>,
}

/// Parse one response body with the explicit nesting guard.
pub(crate) fn parse_response(body: &[u8]) -> Result<Response, SourceError> {
    if !normalize::json_nesting_within(body, normalize::MAX_JSON_NESTING) {
        return Err(SourceError::ExtractionIncomplete);
    }
    let response =
        serde_json::from_slice::<Response>(body).map_err(|_| SourceError::ExtractionIncomplete)?;
    check_code(&response)?;
    Ok(response)
}

/// Validate the documented `code` envelope: `200` is success, `404` is a
/// typed not-found, anything else is a typed API failure.
fn check_code(response: &Response) -> Result<(), SourceError> {
    let Some(code) = response.code.as_deref() else {
        return Err(SourceError::ExtractionIncomplete);
    };
    let code = normalize::parse_u64_raw(code).map_err(|_| SourceError::ExtractionIncomplete)?;
    match code {
        200 => Ok(()),
        404 => Err(SourceError::ProductNotFound),
        _ => Err(SourceError::ApiUnavailable),
    }
}

/// Convert search results into discovery entries, bounded by `limit`.
pub(crate) fn discoveries_from_search(
    response: &Response,
    source: &SourceId,
    ctx: &AcquireCtx,
    limit: u16,
) -> Result<Vec<crate::contract::Discovery>, SourceError> {
    let Some(result) = response.result.as_ref() else {
        return Ok(Vec::new());
    };
    let mut discoveries = Vec::new();
    for product in result.product_list.iter().take(usize::from(limit)) {
        let offer = offer_from_product(product, source, ctx)?;
        discoveries.push(normalize::discovery_from_offer(&offer, None));
    }
    Ok(discoveries)
}

/// The first product of a search response, for MPN-to-product-code
/// resolution.
pub(crate) fn first_product(response: &Response) -> Option<&Product> {
    response
        .result
        .as_ref()
        .and_then(|result| result.product_list.first())
}

/// Normalize one product into the canonical offer.
pub(crate) fn offer_from_product(
    product: &Product,
    source: &SourceId,
    ctx: &AcquireCtx,
) -> Result<CommercialOffer, SourceError> {
    let mut identity = ProductIdentity::empty();
    identity.manufacturer_part_number = match product.product_model.as_deref() {
        Some(mpn) if !mpn.trim().is_empty() => Some(normalize::strict_text::<256>(mpn)?),
        _ => None,
    };
    identity.source_part_number = match product.product_code.as_deref() {
        Some(code) if !code.trim().is_empty() => Some(normalize::strict_text::<256>(code)?),
        _ => None,
    };
    identity.offer_id = identity
        .source_part_number
        .as_ref()
        .and_then(|code| Text::<512>::new(code.as_str()).ok());
    identity.canonical_url = product
        .product_url
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
        product
            .product_desc_en
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(fallback_title),
        fallback_title,
    )?;
    let description = product
        .product_desc_en
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| normalize::display_text::<4096>(value, title.as_str()))
        .transpose()?;
    let manufacturer = product
        .brand_name_en
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            normalize::display_text::<256>(value, "unknown").map(|name| Manufacturer {
                id: None,
                name,
                country: None,
                url: None,
            })
        })
        .transpose()?;

    let (currency, price_breaks, price_visibility) = price_list(product)?;
    let stock = match product.stock_number.as_deref() {
        Some(raw) => match optional_quantity(raw)? {
            Some(quantity) => StockState::InStock { quantity },
            None if raw.get().trim() == "0" => StockState::OutOfStock,
            None => StockState::Unknown,
        },
        None => StockState::Unknown,
    };
    let moq = product
        .min_buy_number
        .as_deref()
        .map(optional_quantity)
        .transpose()?
        .flatten();
    let standard_pack = product
        .standard_pack
        .as_deref()
        .map(optional_quantity)
        .transpose()?
        .flatten();
    let packaging_kind = product
        .packing_type
        .as_deref()
        .and_then(normalize::packaging_from_label);
    let packaging = match packaging_kind {
        Some(kind) => vec![PackagingOption {
            packaging: kind,
            label: product
                .packing_type
                .as_deref()
                .map(normalize::strict_text::<64>)
                .transpose()?,
            moq,
            order_multiple: None,
            standard_pack,
            stock,
            price_breaks: Vec::new(),
            lead_time: None,
        }],
        None => Vec::new(),
    };
    let lead_time = product
        .lead_time
        .as_deref()
        .and_then(normalize::parse_lead_time_label);
    let offer = CommercialOffer {
        source: source.clone(),
        identity,
        title,
        description,
        currency: currency.unwrap_or(Currency::USD),
        price_breaks,
        variants: Vec::new(),
        moq,
        order_multiple: None,
        standard_pack,
        stock,
        lead_time,
        packaging,
        supplier: None,
        manufacturer,
        provenance: normalize::provenance(
            source,
            ctx,
            faktor_commerce::offer::ObservationOrigin::OfficialApi,
        ),
        observed_at_ms: ctx.now_ms(),
        price_visibility,
        lifecycle: normalize::lifecycle_from_label(product.product_status.as_deref().unwrap_or("")),
    };
    offer
        .validate()
        .map_err(|error| normalize::map_offer_error(error).to_source_error())?;
    Ok(offer)
}

/// Select the applicable LCSC price list: the discounted list when present
/// (the offer is then `promotional`), the regular list otherwise.
fn price_list(
    product: &Product,
) -> Result<(Option<Currency>, Vec<PriceBreak>, PriceVisibility), NormalizeError> {
    let fallback = product
        .currency_code
        .as_deref()
        .or(product.currency_symbol.as_deref());
    if !product.product_discount_price_list.is_empty() {
        let (currency, breaks) =
            normalize_price_list(&product.product_discount_price_list, fallback)?;
        return Ok((currency, breaks, PriceVisibility::Promotional));
    }
    let (currency, breaks) = normalize_price_list(&product.product_price_list, fallback)?;
    let visibility = if breaks.is_empty() {
        PriceVisibility::Unknown
    } else {
        PriceVisibility::Public
    };
    Ok((currency, breaks, visibility))
}

fn normalize_price_list(
    raws: &[PriceRaw],
    fallback_currency: Option<&str>,
) -> Result<(Option<Currency>, Vec<PriceBreak>), NormalizeError> {
    if raws.is_empty() {
        return Ok((None, Vec::new()));
    }
    if raws.len() > MAX_PRICE_BREAKS {
        return Err(NormalizeError::Schema);
    }
    let fallback = fallback_currency
        .map(|label| normalize::currency_from_symbol(label).ok_or(NormalizeError::CurrencyConflict))
        .transpose()?;
    let mut currency: Option<Currency> = None;
    let mut breaks = Vec::new();
    for raw in raws {
        let ladder = raw
            .ladder
            .as_deref()
            .ok_or(NormalizeError::InvalidQuantity)?;
        let min_quantity = normalize::nonzero_quantity(normalize::parse_u64_raw(ladder)?)?
            .ok_or(NormalizeError::InvalidQuantity)?;
        let price = raw
            .product_price
            .as_deref()
            .ok_or(NormalizeError::InvalidMoney)?;
        let break_currency = raw
            .currency_code
            .as_deref()
            .or(raw.currency_symbol.as_deref())
            .and_then(normalize::currency_from_symbol)
            .or(fallback)
            .ok_or(NormalizeError::CurrencyConflict)?;
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
    normalize::validate_tiers(&breaks, currency.unwrap_or(Currency::USD))?;
    Ok((currency, breaks))
}

/// Parse an optional quantity token; JSON `null` is `None` (not stated),
/// zero is `None` (the caller distinguishes out-of-stock).
fn optional_quantity(raw: &RawValue) -> Result<Option<NonZeroQuantity>, NormalizeError> {
    if raw.get().trim() == "null" {
        return Ok(None);
    }
    normalize::parse_nonzero_quantity_raw(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport;

    fn source() -> SourceId {
        SourceId::new("lcsc").expect("valid source")
    }

    fn ctx() -> AcquireCtx {
        testsupport::ctx()
    }

    fn detail(fixture: &str) -> CommercialOffer {
        let body = crate::testing::fixture(fixture).expect("fixture");
        let response = parse_response(body.as_bytes()).expect("parse");
        let product = response
            .result
            .as_ref()
            .and_then(|result| result.product_list.first())
            .expect("product");
        offer_from_product(product, &source(), &ctx()).expect("normalize")
    }

    #[test]
    fn regular_price_list_normalizes_with_moq_and_stock() {
        let offer = detail("lcsc/product_details.json");
        assert_eq!(
            offer
                .identity
                .manufacturer_part_number
                .as_ref()
                .map(Text::as_str),
            Some("STM32F407VGT6")
        );
        assert_eq!(
            offer.identity.source_part_number.as_ref().map(Text::as_str),
            Some("C89642")
        );
        assert_eq!(offer.stock.available_quantity(), Some(4321));
        assert_eq!(offer.moq.map(NonZeroQuantity::get), Some(1));
        assert_eq!(offer.price_visibility, PriceVisibility::Public);
        assert_eq!(
            offer.price_breaks[0].unit_price.to_decimal_string(),
            "12.340000"
        );
        assert_eq!(
            offer.lead_time,
            Some(faktor_commerce::LeadTime::new(5, 5).expect("lead"))
        );
        assert_eq!(offer.packaging.len(), 1);
        assert_eq!(
            offer.packaging[0].packaging,
            faktor_commerce::PackagingType::Tray
        );
    }

    #[test]
    fn discounted_list_is_the_applicable_promotional_price() {
        let offer = detail("lcsc/product_details_discount.json");
        assert_eq!(offer.price_visibility, PriceVisibility::Promotional);
        let prices: Vec<String> = offer
            .price_breaks
            .iter()
            .map(|break_| break_.unit_price.to_decimal_string())
            .collect();
        assert_eq!(prices, vec!["9.500000"]);
        assert_eq!(offer.price_breaks[0].min_quantity.get(), 100);
    }

    #[test]
    fn search_results_become_discoveries_bounded_by_limit() {
        let body = crate::testing::fixture("lcsc/keyword_search.json").expect("fixture");
        let response = parse_response(body.as_bytes()).expect("parse");
        let discoveries =
            discoveries_from_search(&response, &source(), &ctx(), 10).expect("normalize");
        assert_eq!(discoveries.len(), 1);
        assert_eq!(
            discoveries[0].price_hint.expect("hint").to_decimal_string(),
            "10.990000"
        );
    }

    #[test]
    fn non_200_envelope_codes_are_typed() {
        let not_found = parse_response(br#"{"code":404,"result":null}"#);
        assert!(matches!(not_found, Err(SourceError::ProductNotFound)));
        let failure = parse_response(br#"{"code":500,"result":null}"#);
        assert!(matches!(failure, Err(SourceError::ApiUnavailable)));
        let hostile = parse_response(br#"{"code":"oops","result":null}"#);
        assert!(matches!(hostile, Err(SourceError::ExtractionIncomplete)));
    }

    #[test]
    fn hostile_payloads_never_misparse_a_price() {
        for fixture in [
            "lcsc/malformed.json",
            "lcsc/wrong_types.json",
            "lcsc/unicode_hostile.json",
            "lcsc/deep_nesting.json",
            "lcsc/huge_price_breaks.json",
        ] {
            let body = crate::testing::fixture(fixture).expect("fixture");
            let outcome = parse_response(body.as_bytes())
                .and_then(|response| discoveries_from_search(&response, &source(), &ctx(), 10));
            assert!(
                matches!(
                    outcome,
                    Err(SourceError::ExtractionIncomplete) | Err(SourceError::ExtractionConflict)
                ),
                "{fixture} must fail typed"
            );
        }
    }
}
