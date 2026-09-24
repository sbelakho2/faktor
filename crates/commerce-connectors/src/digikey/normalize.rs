//! DigiKey Product Information V4 JSON → `faktor-commerce` normalization.
//!
//! Endpoints covered (sanitized fixtures under `fixtures/digikey/`):
//!
//! * `POST /products/v4/search/keyword` — discovery only. The documented
//!   catalog staleness is up to 24 hours, carried on every
//!   [`Discovery::documented_max_staleness_ms`];
//! * `GET /products/v4/search/{product}/productdetails` — the authoritative
//!   product record used for exact lookups and as the live-pricing base;
//! * `POST /products/v4/pricing/{product}/pricing` — PricingOptionsByQuantity,
//!   the live quantity pricing. A live-pricing request never answers from the
//!   KeywordSearch price.
//!
//! Prices are captured as [`RawValue`] and parsed with integer arithmetic.

use faktor_commerce::money::Currency;
use faktor_commerce::offer::{
    CommercialOffer, Manufacturer, PackagingOption, PriceBreak, PriceVisibility, VariantOffer,
};
use faktor_commerce::quantity::NonZeroQuantity;
use faktor_commerce::text::{CanonicalUrl, Text, VariantId};
use faktor_commerce::{ProductIdentity, SourceError, SourceId, StockState};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::context::AcquireCtx;
use crate::normalize::{self, NormalizeError};

/// The documented maximum staleness of DigiKey keyword-search data.
pub const KEYWORD_SEARCH_MAX_STALENESS_MS: u64 = 24 * 60 * 60 * 1000;

/// The documented maximum price breaks accepted from one DigiKey pricing
/// list.
pub const MAX_PRICE_BREAKS: usize = 32;

/// The OAuth client-credentials token response.
// SECRET-FIELD-GATE-WIRE-DTO: DigiKey OAuth token-endpoint provider payload; token_from_response extracts the token into the connector SecretString at the same boundary and the DTO is never stored.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: Option<String>,
    pub(crate) token_type: Option<String>,
    pub(crate) expires_in: Option<Box<RawValue>>,
}

/// One price entry.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PriceRaw {
    #[serde(rename = "BreakQuantity")]
    pub(crate) break_quantity: Option<Box<RawValue>>,
    #[serde(rename = "UnitPrice")]
    pub(crate) unit_price: Option<Box<RawValue>>,
    #[serde(rename = "Currency")]
    pub(crate) currency: Option<String>,
}

/// `PackageType`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PackageTypeRaw {
    #[serde(rename = "Id")]
    pub(crate) id: Option<Box<RawValue>>,
    #[serde(rename = "Name")]
    pub(crate) name: Option<String>,
}

/// `Manufacturer`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ManufacturerRaw {
    #[serde(rename = "Id")]
    pub(crate) id: Option<Box<RawValue>>,
    #[serde(rename = "Name")]
    pub(crate) name: Option<String>,
}

/// `Description`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct DescriptionRaw {
    #[serde(rename = "ProductDescription")]
    pub(crate) product_description: Option<String>,
    #[serde(rename = "DetailedDescription")]
    pub(crate) detailed_description: Option<String>,
}

/// `ProductStatus`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ProductStatusRaw {
    #[serde(rename = "Status")]
    pub(crate) status: Option<String>,
}

/// One product variation (a DigiKey SKU with its packaging and pricing).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct VariationRaw {
    #[serde(rename = "DigiKeyProductNumber")]
    pub(crate) digikey_product_number: Option<String>,
    #[serde(rename = "PackageType")]
    pub(crate) package_type: PackageTypeRaw,
    #[serde(rename = "QuantityAvailable")]
    pub(crate) quantity_available: Option<Box<RawValue>>,
    #[serde(rename = "MinimumOrderQuantity")]
    pub(crate) minimum_order_quantity: Option<Box<RawValue>>,
    #[serde(rename = "StandardPackage")]
    pub(crate) standard_package: Option<Box<RawValue>>,
    #[serde(rename = "StandardPricing")]
    pub(crate) standard_pricing: Vec<PriceRaw>,
    #[serde(rename = "MyPricing")]
    pub(crate) my_pricing: Option<Vec<PriceRaw>>,
}

/// One product.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ProductRaw {
    #[serde(rename = "ManufacturerProductNumber")]
    pub(crate) manufacturer_product_number: Option<String>,
    #[serde(rename = "Description")]
    pub(crate) description: DescriptionRaw,
    #[serde(rename = "Manufacturer")]
    pub(crate) manufacturer: ManufacturerRaw,
    #[serde(rename = "ProductUrl")]
    pub(crate) product_url: Option<String>,
    #[serde(rename = "ProductStatus")]
    pub(crate) product_status: ProductStatusRaw,
    #[serde(rename = "ProductVariations")]
    pub(crate) product_variations: Vec<VariationRaw>,
}

/// The KeywordSearch response.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct KeywordResponse {
    #[serde(rename = "ProductsCount")]
    pub(crate) products_count: Option<Box<RawValue>>,
    #[serde(rename = "Products")]
    pub(crate) products: Vec<ProductRaw>,
}

/// The ProductDetails response.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ProductDetailsResponse {
    #[serde(rename = "Product")]
    pub(crate) product: Option<ProductRaw>,
}

/// The PricingOptionsByQuantity response.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PricingResponse {
    #[serde(rename = "Currency")]
    pub(crate) currency: Option<String>,
    #[serde(rename = "ProductPricing")]
    pub(crate) product_pricing: Vec<PriceRaw>,
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, SourceError> {
    if !normalize::json_nesting_within(body, normalize::MAX_JSON_NESTING) {
        return Err(SourceError::ExtractionIncomplete);
    }
    serde_json::from_slice::<T>(body).map_err(|_| SourceError::ExtractionIncomplete)
}

/// Parse the OAuth token response.
pub(crate) fn parse_token_response(body: &[u8]) -> Result<TokenResponse, SourceError> {
    parse(body)
}

/// Extract the access token and its lifetime in milliseconds. A token
/// response without a usable token is `AuthenticationRequired`; the token
/// itself is wrapped so it can never be printed.
pub(crate) fn token_from_response(response: &TokenResponse) -> Result<(String, u64), SourceError> {
    let token = response
        .access_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(SourceError::AuthenticationRequired)?;
    let expires_ms = match response.expires_in.as_deref() {
        Some(raw) => normalize::parse_u64_raw(raw)
            .map_err(|_| SourceError::AuthenticationRequired)?
            .saturating_mul(1000),
        None => 0,
    };
    Ok((token.to_string(), expires_ms))
}

/// Parse a KeywordSearch response.
pub(crate) fn parse_keyword_response(body: &[u8]) -> Result<KeywordResponse, SourceError> {
    parse(body)
}

/// Parse a ProductDetails response and require its product record.
pub(crate) fn parse_product_details(body: &[u8]) -> Result<ProductRaw, SourceError> {
    parse::<ProductDetailsResponse>(body)?
        .product
        .ok_or(SourceError::ProductNotFound)
}

/// Parse a PricingOptionsByQuantity response.
pub(crate) fn parse_pricing_response(body: &[u8]) -> Result<PricingResponse, SourceError> {
    parse(body)
}

/// Convert keyword-search products into discovery results. Bounded by
/// `limit`; each carries the documented 24-hour staleness.
pub(crate) fn discoveries_from_keyword(
    response: &KeywordResponse,
    source: &SourceId,
    ctx: &AcquireCtx,
    limit: u16,
) -> Result<Vec<crate::contract::Discovery>, SourceError> {
    let mut discoveries = Vec::new();
    for product in response.products.iter().take(usize::from(limit)) {
        let offer = offer_from_product(product, None, source, ctx)?;
        discoveries.push(normalize::discovery_from_offer(
            &offer,
            Some(KEYWORD_SEARCH_MAX_STALENESS_MS),
        ));
    }
    Ok(discoveries)
}

/// Normalize one product record. `pricing` is the live pricing response when
/// live pricing was requested; it replaces the product record's own price
/// list and is never mixed with it.
pub(crate) fn offer_from_product(
    product: &ProductRaw,
    pricing: Option<&PricingResponse>,
    source: &SourceId,
    ctx: &AcquireCtx,
) -> Result<CommercialOffer, SourceError> {
    let primary = product.product_variations.first();
    let mut identity = ProductIdentity::empty();
    identity.manufacturer_part_number = match product.manufacturer_product_number.as_deref() {
        Some(mpn) if !mpn.trim().is_empty() => Some(normalize::strict_text::<256>(mpn)?),
        _ => None,
    };
    identity.source_part_number = primary
        .and_then(|variation| variation.digikey_product_number.as_deref())
        .filter(|value| !value.trim().is_empty())
        .map(normalize::strict_text::<256>)
        .transpose()?;
    identity.offer_id = identity
        .source_part_number
        .as_ref()
        .map(|source_part| {
            Text::<512>::new(source_part.as_str()).map_err(|_| NormalizeError::IdentityText)
        })
        .transpose()?;
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
            .description
            .product_description
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(fallback_title),
        fallback_title,
    )?;
    let description = product
        .description
        .detailed_description
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| normalize::display_text::<4096>(value, title.as_str()))
        .transpose()?;
    let manufacturer = product
        .manufacturer
        .name
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

    // Live pricing, when requested, comes only from the pricing response.
    let (pricing_currency, live_breaks) = match pricing {
        Some(pricing) => {
            normalize_price_list(&pricing.product_pricing, pricing.currency.as_deref())?
        }
        None => (None, Vec::new()),
    };
    let (primary_currency, primary_breaks) = match primary {
        Some(variation) => normalize_price_list(&variation.standard_pricing, None)?,
        None => (None, Vec::new()),
    };
    let (currency, price_breaks) = if pricing.is_some() {
        (pricing_currency, live_breaks)
    } else {
        (primary_currency, primary_breaks)
    };

    let quantity_available = primary
        .and_then(|variation| variation.quantity_available.as_deref())
        .map(quantity_token)
        .transpose()?
        .flatten();
    let stock = match quantity_available {
        Some(quantity) => StockState::InStock { quantity },
        None => match primary.and_then(|variation| variation.quantity_available.as_deref()) {
            Some(raw) if normalize::raw_token_text(raw).as_deref() == Ok("0") => {
                StockState::OutOfStock
            }
            _ => StockState::Unknown,
        },
    };
    let moq = primary
        .and_then(|variation| variation.minimum_order_quantity.as_deref())
        .map(quantity_token)
        .transpose()?
        .flatten();
    let standard_pack = primary
        .and_then(|variation| variation.standard_package.as_deref())
        .map(quantity_token)
        .transpose()?
        .flatten();
    let packaging_kind = primary
        .and_then(|variation| variation.package_type.name.as_deref())
        .and_then(normalize::packaging_from_label);

    let variants = if product.product_variations.len() > 1 {
        normalize_variants(&product.product_variations, pricing.is_some())?
    } else {
        Vec::new()
    };
    let packaging = match packaging_kind {
        Some(kind) => vec![PackagingOption {
            packaging: kind,
            label: primary
                .and_then(|variation| variation.package_type.name.as_deref())
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
        currency: currency.unwrap_or(Currency::USD),
        price_breaks,
        variants,
        moq,
        order_multiple: None,
        standard_pack,
        stock,
        lead_time: None,
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
        lifecycle: normalize::lifecycle_from_label(
            product.product_status.status.as_deref().unwrap_or(""),
        ),
    };
    offer
        .validate()
        .map_err(|error| normalize::map_offer_error(error).to_source_error())?;
    Ok(offer)
}

fn normalize_variants(
    variations: &[VariationRaw],
    suppress_prices: bool,
) -> Result<Vec<VariantOffer>, SourceError> {
    let mut variants = Vec::new();
    for variation in variations {
        let Some(variant_id) = variation.digikey_product_number.as_deref() else {
            continue;
        };
        let variant_id = VariantId::new(variant_id).map_err(|_| NormalizeError::IdentityText)?;
        let (_, variant_breaks) = if suppress_prices {
            (None, Vec::new())
        } else {
            normalize_price_list(&variation.standard_pricing, None)?
        };
        let quantity = variation
            .quantity_available
            .as_deref()
            .map(quantity_token)
            .transpose()?
            .flatten();
        let stock = match quantity {
            Some(quantity) => StockState::InStock { quantity },
            None => StockState::Unknown,
        };
        let moq = variation
            .minimum_order_quantity
            .as_deref()
            .map(quantity_token)
            .transpose()?
            .flatten();
        variants.push(VariantOffer {
            variant_id,
            attributes: Vec::new(),
            packaging: variation
                .package_type
                .name
                .as_deref()
                .and_then(normalize::packaging_from_label),
            moq,
            order_multiple: None,
            standard_pack: variation
                .standard_package
                .as_deref()
                .map(quantity_token)
                .transpose()?
                .flatten(),
            stock,
            price_breaks: variant_breaks,
            lead_time: None,
        });
    }
    Ok(variants)
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
        let quantity = raw
            .break_quantity
            .as_deref()
            .ok_or(NormalizeError::InvalidQuantity)?;
        let min_quantity = normalize::nonzero_quantity(normalize::parse_u64_raw(quantity)?)?
            .ok_or(NormalizeError::InvalidQuantity)?;
        let price = raw
            .unit_price
            .as_deref()
            .ok_or(NormalizeError::InvalidMoney)?;
        let break_currency = match raw.currency.as_deref() {
            Some(label) => normalize::currency_from_symbol(label),
            None => fallback,
        }
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

/// Parse an optional quantity token; JSON `null` is `None` (the source did
/// not state a quantity), never zero.
fn quantity_token(raw: &RawValue) -> Result<Option<NonZeroQuantity>, NormalizeError> {
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
        SourceId::new("digikey").expect("valid source")
    }

    fn ctx() -> AcquireCtx {
        testsupport::ctx()
    }

    #[test]
    fn keyword_search_carries_documented_staleness_and_a_price_hint() {
        let body = crate::testing::fixture("digikey/keyword_search.json").expect("fixture");
        let response = parse_keyword_response(body.as_bytes()).expect("parse");
        let discoveries =
            discoveries_from_keyword(&response, &source(), &ctx(), 10).expect("normalize");
        assert_eq!(discoveries.len(), 1);
        assert_eq!(
            discoveries[0].documented_max_staleness_ms,
            Some(KEYWORD_SEARCH_MAX_STALENESS_MS)
        );
        assert_eq!(
            discoveries[0].price_hint.expect("hint").to_decimal_string(),
            "0.850000"
        );
        assert_eq!(
            discoveries[0]
                .identity
                .manufacturer_part_number
                .as_ref()
                .map(Text::as_str),
            Some("TPS5430DDAR")
        );
    }

    #[test]
    fn product_details_normalize_terms_and_packaging() {
        let body = crate::testing::fixture("digikey/product_details.json").expect("fixture");
        let product = parse_product_details(body.as_bytes()).expect("parse");
        let offer = offer_from_product(&product, None, &source(), &ctx()).expect("normalize");
        assert_eq!(offer.stock.available_quantity(), Some(4321));
        assert_eq!(offer.moq.map(NonZeroQuantity::get), Some(1));
        assert_eq!(offer.packaging.len(), 1);
        assert_eq!(
            offer.packaging[0].packaging,
            faktor_commerce::PackagingType::Tube
        );
        assert_eq!(offer.price_breaks.len(), 2);
        assert_eq!(
            offer.price_breaks[0].unit_price.to_decimal_string(),
            "5.000000"
        );
        assert_eq!(offer.lifecycle, faktor_commerce::LifecycleStatus::Active);
    }

    #[test]
    fn live_pricing_response_replaces_the_product_price_list() {
        let details = crate::testing::fixture("digikey/product_details.json").expect("fixture");
        let pricing = crate::testing::fixture("digikey/pricing_by_quantity.json").expect("fixture");
        let product = parse_product_details(details.as_bytes()).expect("parse");
        let pricing = parse_pricing_response(pricing.as_bytes()).expect("parse");
        let offer =
            offer_from_product(&product, Some(&pricing), &source(), &ctx()).expect("normalize");
        let prices: Vec<String> = offer
            .price_breaks
            .iter()
            .map(|break_| break_.unit_price.to_decimal_string())
            .collect();
        assert_eq!(prices, vec!["5.000000", "4.500000"]);
        assert!(
            offer
                .price_breaks
                .iter()
                .all(|break_| break_.unit_price.to_decimal_string() != "1.000000"),
            "a discovery price must never survive into a live-priced offer"
        );
    }

    #[test]
    fn multiple_variations_are_preserved_and_never_collapsed_to_the_cheapest() {
        let body =
            crate::testing::fixture("digikey/product_details_variants.json").expect("fixture");
        let product = parse_product_details(body.as_bytes()).expect("parse");
        let offer = offer_from_product(&product, None, &source(), &ctx()).expect("normalize");
        assert_eq!(offer.variants.len(), 2);
        assert_eq!(
            offer.variants[0].packaging,
            Some(faktor_commerce::PackagingType::CutTape)
        );
        assert_eq!(
            offer.variants[1].packaging,
            Some(faktor_commerce::PackagingType::TapeAndReel)
        );
        assert_eq!(offer.variants[1].moq.map(NonZeroQuantity::get), Some(2500));
        // The ambiguous mapping is preserved for the quote resolver instead
        // of silently picking the cheapest SKU.
        let resolution = faktor_commerce::price_at_quantity(
            &offer,
            faktor_commerce::VariantRequest::None,
            faktor_commerce::Quantity::new(100).expect("quantity"),
        );
        assert_eq!(
            resolution.status,
            faktor_commerce::QuoteStatus::VariantAmbiguous
        );
    }

    #[test]
    fn huge_product_arrays_are_bounded_by_the_requested_limit() {
        let products: Vec<serde_json::Value> = (0..2048)
            .map(|index| {
                serde_json::json!({
                    "ManufacturerProductNumber": format!("PART{index:06}"),
                    "Description": { "ProductDescription": "Buck regulator" },
                    "ProductVariations": [{
                        "DigiKeyProductNumber": format!("296-{index:06}-1-ND"),
                        "QuantityAvailable": 1,
                        "MinimumOrderQuantity": 1,
                        "StandardPricing": [
                            { "BreakQuantity": 1, "UnitPrice": "1.00", "Currency": "USD" }
                        ]
                    }]
                })
            })
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({
            "ProductsCount": 2048,
            "Products": products
        }))
        .expect("serialize");
        let response = parse_keyword_response(&body).expect("parse");
        let discoveries =
            discoveries_from_keyword(&response, &source(), &ctx(), 2).expect("normalize");
        assert_eq!(discoveries.len(), 2);
    }

    #[test]
    fn hostile_payloads_are_typed_errors() {
        for fixture in [
            "digikey/malformed.json",
            "digikey/wrong_types.json",
            "digikey/unicode_hostile.json",
            "digikey/deep_nesting.json",
        ] {
            let body = crate::testing::fixture(fixture).expect("fixture");
            let outcome = parse_keyword_response(body.as_bytes())
                .and_then(|response| discoveries_from_keyword(&response, &source(), &ctx(), 10));
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
