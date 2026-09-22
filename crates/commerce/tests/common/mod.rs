#![allow(dead_code)]

use faktor_commerce::*;

pub fn text<const MAX: usize>(value: &str) -> Text<MAX> {
    Text::new(value).expect("fixture text is valid")
}

pub fn money(amount: &str) -> Money {
    Money::parse(Currency::CNY, amount).expect("fixture money is valid")
}

pub fn usd(amount: &str) -> Money {
    Money::parse(Currency::USD, amount).expect("fixture money is valid")
}

pub fn qty(value: u64) -> Quantity {
    Quantity::new(value).expect("fixture quantity is valid")
}

pub fn nz(value: u64) -> NonZeroQuantity {
    NonZeroQuantity::new(value).expect("fixture quantity is valid")
}

pub fn source(id: &str) -> SourceId {
    SourceId::new(id).expect("fixture source is valid")
}

pub fn provenance(origin: ObservationOrigin) -> OfferProvenance {
    OfferProvenance {
        origin,
        source: source("lcsc"),
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

pub fn identity(mpn: &str, manufacturer: &str) -> ProductIdentity {
    ProductIdentity {
        manufacturer: Some(text(manufacturer)),
        manufacturer_part_number: Some(text(mpn)),
        source_part_number: Some(text("C1234")),
        offer_id: Some(text("offer-1")),
        canonical_url: None,
        category: None,
    }
}

pub fn base_offer() -> CommercialOffer {
    CommercialOffer {
        source: source("lcsc"),
        identity: identity("STM32F407VGT6", "STMicroelectronics"),
        title: text("STM32F407VGT6 LQFP100 microcontroller"),
        description: None,
        currency: Currency::CNY,
        price_breaks: Vec::new(),
        variants: Vec::new(),
        moq: None,
        order_multiple: None,
        standard_pack: None,
        stock: StockState::InStock {
            quantity: nz(1_000_000),
        },
        lead_time: None,
        packaging: Vec::new(),
        supplier: None,
        manufacturer: None,
        provenance: provenance(ObservationOrigin::OfficialApi),
        observed_at_ms: 1_700_000_000_000,
        price_visibility: PriceVisibility::Public,
        lifecycle: LifecycleStatus::Active,
    }
}

pub fn tier(min: u64, price: &str) -> PriceBreak {
    PriceBreak {
        min_quantity: nz(min),
        max_quantity: None,
        unit_price: money(price),
        visibility: PriceVisibility::Public,
        account_scope: None,
        promotion: None,
    }
}

pub fn tier_range(min: u64, max: u64, price: &str) -> PriceBreak {
    PriceBreak {
        max_quantity: Some(nz(max)),
        ..tier(min, price)
    }
}

pub fn with_tiers(mut offer: CommercialOffer, tiers: Vec<PriceBreak>) -> CommercialOffer {
    offer.price_breaks = tiers;
    offer
}

pub fn variant(id: &str, packaging: Option<PackagingType>, tiers: Vec<PriceBreak>) -> VariantOffer {
    VariantOffer {
        variant_id: VariantId::new(id).expect("variant id"),
        attributes: Vec::new(),
        packaging,
        moq: None,
        order_multiple: None,
        standard_pack: None,
        stock: StockState::Unknown,
        price_breaks: tiers,
        lead_time: None,
    }
}

pub fn attribute(name: &str, value: &str) -> VariantAttribute {
    VariantAttribute {
        name: text(name),
        value: text(value),
    }
}

pub fn packaging_option(packaging: PackagingType, tiers: Vec<PriceBreak>) -> PackagingOption {
    PackagingOption {
        packaging,
        label: None,
        moq: None,
        order_multiple: None,
        standard_pack: None,
        stock: StockState::Unknown,
        price_breaks: tiers,
        lead_time: None,
    }
}

/// The spec example: MOQ 100, order multiple 100, tiers
/// `100+ ¥20.00`, `1000+ ¥19.00`, `5000+ ¥18.20`.
pub fn spec_example_offer() -> CommercialOffer {
    let mut offer = with_tiers(
        base_offer(),
        vec![tier(100, "20.00"), tier(1000, "19.00"), tier(5000, "18.20")],
    );
    offer.moq = Some(nz(100));
    offer.order_multiple = Some(nz(100));
    offer
}

pub fn resolved(resolution: &QuoteResolution) -> &Quote {
    assert_eq!(
        resolution.status,
        QuoteStatus::Resolved,
        "expected a resolved quote, got {resolution:?}"
    );
    resolution.quote.as_ref().expect("resolved quote")
}
