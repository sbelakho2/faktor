//! Adversarial tests for deterministic quote resolution (`docs/acquire.md`
//! §7 and §17 step 3): tier boundaries, MOQ, order multiple, package
//! quantity, packaging and variant distinctness, ambiguity, promotions,
//! account pricing, and the unknown-vs-zero rule.

mod common;

use common::*;
use faktor_commerce::*;

fn ctx() -> PricingContext<'static> {
    PricingContext::default()
}

#[test]
fn spec_example_resolves_5000_times_18_20_exactly() {
    let offer = spec_example_offer();
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(5_000));
    let quote = resolved(&resolution);
    assert_eq!(resolution.billed_quantity, Some(qty(5_000)));
    assert_eq!(resolution.charged_quantity, Some(qty(5_000)));
    assert_eq!(
        resolution.unit_price.expect("unit price"),
        Money::from_micros(Currency::CNY, 18_200_000)
    );
    assert_eq!(
        quote.merchandise,
        Money::from_micros(Currency::CNY, 91_000_000_000)
    );
    assert_eq!(quote.merchandise.to_decimal_string(), "91000.000000");
    assert_eq!(quote.total, quote.merchandise);
    assert!(resolution.notes.is_empty());
}

#[test]
fn tier_boundary_table_is_exact() {
    let offer = spec_example_offer();
    // (requested, expected status, expected billed, expected unit, expected merchandise)
    type Expected = Option<(u64, &'static str, &'static str)>;
    let cases: &[(u64, Expected)] = &[
        (1, None),
        (99, None),
        (100, Some((100, "20.00", "2000.00"))),
        (101, Some((200, "20.00", "4000.00"))),
        (999, Some((1000, "19.00", "19000.00"))),
        (1000, Some((1000, "19.00", "19000.00"))),
        (1001, Some((1100, "19.00", "20900.00"))),
        (4999, Some((5000, "18.20", "91000.00"))),
        (5000, Some((5000, "18.20", "91000.00"))),
        (5001, Some((5100, "18.20", "92820.00"))),
    ];
    for (requested, expected) in cases {
        let resolution = price_at_quantity(&offer, VariantRequest::None, qty(*requested));
        match expected {
            None => {
                assert_eq!(
                    resolution.status,
                    QuoteStatus::MoqViolation,
                    "quantity {requested}"
                );
                assert!(resolution.quote.is_none(), "never a fabricated quote");
                assert_eq!(resolution.moq, Some(nz(100)));
            }
            Some((billed, unit, merchandise)) => {
                let quote = resolved(&resolution);
                assert_eq!(
                    resolution.billed_quantity,
                    Some(qty(*billed)),
                    "qty {requested}"
                );
                assert_eq!(
                    resolution.unit_price.expect("unit"),
                    money(unit),
                    "qty {requested}"
                );
                assert_eq!(
                    quote.merchandise,
                    money(merchandise),
                    "qty {requested}: exact micros"
                );
            }
        }
    }
}

#[test]
fn order_multiple_rounds_up_and_is_recorded() {
    let mut offer = with_tiers(base_offer(), vec![tier(1, "10.00")]);
    offer.order_multiple = Some(nz(100));
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(150));
    let quote = resolved(&resolution);
    assert_eq!(resolution.billed_quantity, Some(qty(200)));
    assert_eq!(quote.merchandise, money("2000.00"));
    assert!(resolution
        .notes
        .contains(&QuoteNote::RoundedUpToOrderMultiple));
    assert!(resolution
        .notes
        .contains(&QuoteNote::TierSelectedByBilledQuantity));

    let exact = price_at_quantity(&offer, VariantRequest::None, qty(200));
    assert_eq!(exact.billed_quantity, Some(qty(200)));
    assert!(
        exact.notes.is_empty(),
        "no rounding note at an exact multiple"
    );
}

#[test]
fn standard_pack_rounds_up_with_its_own_note() {
    let mut offer = with_tiers(base_offer(), vec![tier(1, "10.00")]);
    offer.standard_pack = Some(nz(30));
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(100));
    let quote = resolved(&resolution);
    assert_eq!(resolution.billed_quantity, Some(qty(120)));
    assert_eq!(quote.merchandise, money("1200.00"));
    assert!(resolution
        .notes
        .contains(&QuoteNote::RoundedUpToStandardPack));
    assert!(!resolution
        .notes
        .contains(&QuoteNote::RoundedUpToOrderMultiple));
}

#[test]
fn order_multiple_and_standard_pack_use_the_least_common_multiple() {
    let mut offer = with_tiers(base_offer(), vec![tier(1, "10.00")]);
    offer.order_multiple = Some(nz(100));
    offer.standard_pack = Some(nz(30));
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(100));
    let quote = resolved(&resolution);
    assert_eq!(
        resolution.billed_quantity,
        Some(qty(300)),
        "lcm(100, 30) = 300"
    );
    assert_eq!(quote.merchandise, money("3000.00"));
}

#[test]
fn moq_above_a_multiple_rounds_up_to_the_next_purchasable_quantity() {
    let mut offer = with_tiers(base_offer(), vec![tier(1, "10.00")]);
    offer.moq = Some(nz(150));
    offer.order_multiple = Some(nz(100));
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(150));
    assert_eq!(resolution.status, QuoteStatus::Resolved);
    assert_eq!(resolution.billed_quantity, Some(qty(200)));

    let below = price_at_quantity(&offer, VariantRequest::None, qty(149));
    assert_eq!(below.status, QuoteStatus::MoqViolation);
    assert_eq!(below.moq, Some(nz(150)));
    assert!(below.quote.is_none());
}

#[test]
fn tiers_with_ranges_are_respected_at_their_edges() {
    let offer = with_tiers(
        base_offer(),
        vec![
            tier_range(100, 999, "20.00"),
            tier_range(1000, 4999, "19.00"),
            tier(5000, "18.20"),
        ],
    );
    for (quantity, unit) in [
        (100u64, "20.00"),
        (999, "20.00"),
        (1000, "19.00"),
        (4999, "19.00"),
        (5000, "18.20"),
    ] {
        let resolution = price_at_quantity(&offer, VariantRequest::None, qty(quantity));
        assert_eq!(
            resolution.unit_price.expect("unit"),
            money(unit),
            "quantity {quantity}"
        );
    }
}

#[test]
fn a_gap_between_tier_ranges_is_a_typed_no_price() {
    let offer = with_tiers(
        base_offer(),
        vec![tier_range(100, 500, "20.00"), tier(1000, "19.00")],
    );
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(600));
    assert_eq!(resolution.status, QuoteStatus::NoPrice);
    assert!(resolution.quote.is_none());
}

#[test]
fn packaging_options_are_first_class_and_never_merged() {
    let mut offer = base_offer();
    offer.packaging = vec![
        packaging_option(
            PackagingType::TapeAndReel,
            vec![tier(5000, "18.20"), tier(10_000, "17.00")],
        ),
        packaging_option(PackagingType::CutTape, vec![tier(100, "20.00")]),
    ];

    let ambiguous = price_at_quantity(&offer, VariantRequest::None, qty(5000));
    assert_eq!(ambiguous.status, QuoteStatus::PackagingAmbiguous);
    assert!(ambiguous.quote.is_none());
    assert_eq!(
        ambiguous.packaging_candidates,
        vec![PackagingType::TapeAndReel, PackagingType::CutTape]
    );
    let range = ambiguous.price_range.expect("range");
    assert_eq!(range.min, money("18.20"));
    assert_eq!(range.max, money("20.00"));

    let reel_ctx = PricingContext {
        packaging: Some(PackagingType::TapeAndReel),
        ..ctx()
    };
    let resolution = resolve_quote(&offer, &reel_ctx, qty(5000));
    let quote = resolved(&resolution);
    assert_eq!(resolution.packaging, Some(PackagingType::TapeAndReel));
    assert_eq!(quote.merchandise, money("91000.00"));
    assert_eq!(resolution.tier.expect("tier").min_quantity, nz(5000));

    let missing_ctx = PricingContext {
        packaging: Some(PackagingType::Tray),
        ..ctx()
    };
    assert_eq!(
        resolve_quote(&offer, &missing_ctx, qty(5000)).status,
        QuoteStatus::PackagingNotFound
    );
}

#[test]
fn packaging_terms_override_offer_terms() {
    let mut offer = with_tiers(base_offer(), vec![tier(100, "25.00")]);
    offer.moq = Some(nz(100));
    offer.order_multiple = Some(nz(100));
    let mut reel = packaging_option(PackagingType::TapeAndReel, vec![tier(5000, "18.20")]);
    reel.moq = Some(nz(5000));
    reel.order_multiple = Some(nz(5000));
    reel.stock = StockState::InStock {
        quantity: nz(9_000),
    };
    offer.packaging = vec![reel];

    let reel_ctx = PricingContext {
        packaging: Some(PackagingType::TapeAndReel),
        ..ctx()
    };
    // Below the packaging MOQ even though above the offer MOQ.
    let below = resolve_quote(&offer, &reel_ctx, qty(1000));
    assert_eq!(below.status, QuoteStatus::MoqViolation);
    assert_eq!(below.moq, Some(nz(5000)));

    let resolution = resolve_quote(&offer, &reel_ctx, qty(5000));
    let quote = resolved(&resolution);
    assert_eq!(resolution.unit_price.expect("unit"), money("18.20"));
    assert_eq!(quote.merchandise, money("91000.00"));
    assert_eq!(resolution.order_multiple, Some(nz(5000)));

    // The offer-level terms are untouched for an offer-level request.
    let mut without_packaging = offer.clone();
    without_packaging.packaging = Vec::new();
    let offer_level = price_at_quantity(&without_packaging, VariantRequest::None, qty(1000));
    assert_eq!(resolved(&offer_level).merchandise, money("25000.00"));
}

#[test]
fn duplicate_packaging_of_one_type_is_ambiguous_not_an_arbitrary_pick() {
    let mut offer = base_offer();
    offer.packaging = vec![
        packaging_option(PackagingType::TapeAndReel, vec![tier(100, "20.00")]),
        packaging_option(PackagingType::TapeAndReel, vec![tier(100, "15.00")]),
    ];
    let selection = PricingContext {
        packaging: Some(PackagingType::TapeAndReel),
        ..ctx()
    };
    let resolution = resolve_quote(&offer, &selection, qty(100));
    assert_eq!(resolution.status, QuoteStatus::PackagingAmbiguous);
    assert!(resolution.quote.is_none());
    let range = resolution.price_range.expect("range");
    assert_eq!(range.min, money("15.00"));
    assert_eq!(range.max, money("20.00"));
}

#[test]
fn variants_are_distinct_purchasable_items() {
    let mut offer = base_offer();
    offer.variants = vec![
        variant(
            "v-reel",
            Some(PackagingType::TapeAndReel),
            vec![tier(5000, "18.20")],
        ),
        variant(
            "v-tray",
            Some(PackagingType::Tray),
            vec![tier(100, "20.00")],
        ),
    ];

    let by_id = price_at_quantity(&offer, VariantRequest::Id("v-reel"), qty(5000));
    let quote = resolved(&by_id);
    assert_eq!(
        by_id.variant_id,
        Some(VariantId::new("v-reel").expect("id"))
    );
    assert_eq!(quote.merchandise, money("91000.00"));

    assert_eq!(
        price_at_quantity(&offer, VariantRequest::Id("v-missing"), qty(5000)).status,
        QuoteStatus::VariantNotFound
    );

    let mismatch = PricingContext {
        variant: VariantRequest::Id("v-reel"),
        packaging: Some(PackagingType::Tray),
        ..ctx()
    };
    assert_eq!(
        resolve_quote(&offer, &mismatch, qty(5000)).status,
        QuoteStatus::PackagingMismatch
    );
}

#[test]
fn ambiguous_variant_mapping_never_selects_the_cheapest_sku() {
    // The §16 acceptance case: a headline range of ¥2.00–¥36.00 must never
    // be quoted as ¥2.00 when the caller asked for the ¥36.00 variant.
    let mut offer = base_offer();
    offer.variants = vec![
        variant("v-cheap", None, vec![tier(1, "2.00")]),
        variant("v-wanted", None, vec![tier(1, "36.00")]),
    ];
    let mut v_cheap = offer.variants[0].clone();
    v_cheap.attributes = vec![attribute("grade", "commercial")];
    let mut v_wanted = offer.variants[1].clone();
    v_wanted.attributes = vec![attribute("grade", "industrial")];
    offer.variants = vec![v_cheap, v_wanted];

    let unspecified = price_at_quantity(&offer, VariantRequest::None, qty(100));
    assert_eq!(unspecified.status, QuoteStatus::VariantAmbiguous);
    assert!(unspecified.quote.is_none(), "no SKU is silently selected");
    assert_eq!(unspecified.candidates.len(), 2);
    let range = unspecified.price_range.expect("range");
    assert_eq!(range.min, money("2.00"));
    assert_eq!(range.max, money("36.00"));

    let by_attributes = price_at_quantity(
        &offer,
        VariantRequest::Attributes(&[attribute("grade", "industrial")]),
        qty(100),
    );
    let quote = resolved(&by_attributes);
    assert_eq!(
        by_attributes.variant_id,
        Some(VariantId::new("v-wanted").expect("id"))
    );
    assert_eq!(quote.merchandise, money("3600.00"));
    assert_ne!(quote.merchandise, money("200.00"));

    // Attribute matching that hits both candidates stays ambiguous.
    let both = price_at_quantity(&offer, VariantRequest::Attributes(&[]), qty(100));
    assert_eq!(both.status, QuoteStatus::VariantAmbiguous);
    assert!(both.quote.is_none());
}

#[test]
fn offer_level_prices_apply_when_variants_carry_no_prices() {
    let mut offer = spec_example_offer();
    offer.variants = vec![
        variant("colour-black", None, Vec::new()),
        variant("colour-white", None, Vec::new()),
    ];
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(5000));
    let quote = resolved(&resolution);
    assert_eq!(quote.merchandise, money("91000.00"));
    assert!(resolution.candidates.is_empty());
}

#[test]
fn explicit_promotions_are_integer_exact() {
    let mut percent = with_tiers(base_offer(), vec![tier(100, "20.00")]);
    percent.price_breaks[0].promotion = Some(Promotion {
        id: text("promo-5"),
        kind: PromotionKind::PercentOff {
            basis_points: BasisPoints::new(500).expect("bp"),
        },
        valid_until_ms: None,
        label: None,
    });
    let resolution = price_at_quantity(&percent, VariantRequest::None, qty(100));
    let quote = resolved(&resolution);
    assert_eq!(resolution.unit_price.expect("unit"), money("19.00"));
    assert_eq!(resolution.list_unit_price.expect("list"), money("20.00"));
    assert_eq!(quote.merchandise, money("1900.00"));
    assert_eq!(resolution.applied_promotions.len(), 1);
    assert_eq!(resolution.applied_promotions[0].saved, money("100.00"));
    assert!(resolution.notes.contains(&QuoteNote::PromotionApplied));

    let mut amount_off = with_tiers(base_offer(), vec![tier(100, "20.00")]);
    amount_off.price_breaks[0].promotion = Some(Promotion {
        id: text("promo-1"),
        kind: PromotionKind::AmountOff {
            per_unit: money("1.00"),
        },
        valid_until_ms: None,
        label: None,
    });
    let amount_off_resolution = price_at_quantity(&amount_off, VariantRequest::None, qty(100));
    let quote = resolved(&amount_off_resolution);
    assert_eq!(quote.merchandise, money("1900.00"));

    let mut free = with_tiers(base_offer(), vec![tier(100, "20.00")]);
    free.price_breaks[0].promotion = Some(Promotion {
        id: text("promo-free"),
        kind: PromotionKind::FreeUnits {
            pay_units: nz(9),
            free_units: nz(1),
        },
        valid_until_ms: None,
        label: None,
    });
    let resolution = price_at_quantity(&free, VariantRequest::None, qty(100));
    let quote = resolved(&resolution);
    assert_eq!(resolution.charged_quantity, Some(qty(90)));
    assert_eq!(quote.merchandise, money("1800.00"));
    assert_eq!(resolution.unit_price.expect("unit"), money("20.00"));

    let mut bundle = with_tiers(base_offer(), vec![tier(100, "20.00")]);
    bundle.price_breaks[0].promotion = Some(Promotion {
        id: text("promo-bundle"),
        kind: PromotionKind::BundlePrice {
            quantity: nz(100),
            price: money("1900.00"),
        },
        valid_until_ms: None,
        label: None,
    });
    let resolution = price_at_quantity(&bundle, VariantRequest::None, qty(250));
    let quote = resolved(&resolution);
    assert_eq!(
        quote.merchandise,
        money("4800.00"),
        "2 bundles at 1900.00 plus 50 loose units at 20.00"
    );
    assert!(
        resolution.unit_price.is_none(),
        "a bundle promotion has no uniform unit price"
    );
    assert_eq!(resolution.list_unit_price.expect("list"), money("20.00"));
}

#[test]
fn expired_promotions_are_recorded_and_not_applied() {
    let mut offer = with_tiers(base_offer(), vec![tier(100, "20.00")]);
    offer.price_breaks[0].promotion = Some(Promotion {
        id: text("promo-expired"),
        kind: PromotionKind::PercentOff {
            basis_points: BasisPoints::new(5_000).expect("bp"),
        },
        valid_until_ms: Some(1_000),
        label: None,
    });
    let active_ctx = PricingContext {
        now_ms: 999,
        ..ctx()
    };
    let active = resolve_quote(&offer, &active_ctx, qty(100));
    assert_eq!(active.unit_price.expect("unit"), money("10.00"));

    let expired_ctx = PricingContext {
        now_ms: 1_000,
        ..ctx()
    };
    let resolution = resolve_quote(&offer, &expired_ctx, qty(100));
    let quote = resolved(&resolution);
    assert_eq!(resolution.unit_price.expect("unit"), money("20.00"));
    assert_eq!(quote.merchandise, money("2000.00"));
    assert!(resolution.notes.contains(&QuoteNote::PromotionExpired));
    assert!(resolution.applied_promotions.is_empty());

    let disabled_ctx = PricingContext {
        apply_promotions: false,
        ..ctx()
    };
    let disabled = resolve_quote(&offer, &disabled_ctx, qty(100));
    assert_eq!(disabled.unit_price.expect("unit"), money("20.00"));
    assert!(!disabled.notes.contains(&QuoteNote::PromotionApplied));
}

#[test]
fn account_pricing_requires_the_matching_scope_and_wins_at_equal_tiers() {
    let mut offer = with_tiers(base_offer(), vec![tier(100, "20.00")]);
    let mut account_tier = tier(100, "17.00");
    account_tier.visibility = PriceVisibility::AccountSpecific;
    account_tier.account_scope = Some(AccountScope::new("acct-1").expect("scope"));
    offer.price_breaks.push(account_tier);

    // The scoped double minimum is a FIRST-CLASS model state: it survives
    // the real deserialization path (offer -> JSON -> CommercialOffer).
    let json = serde_json::to_value(&offer).expect("serialize");
    let offer: CommercialOffer =
        serde_json::from_value(json).expect("public + account tier at the same minimum");

    let public = price_at_quantity(&offer, VariantRequest::None, qty(100));
    let public_quote = resolved(&public);
    assert_eq!(public_quote.merchandise, money("2000.00"));
    assert!(!public.notes.contains(&QuoteNote::AccountPriceApplied));

    let scope = AccountScope::new("acct-1").expect("scope");
    let account_ctx = PricingContext {
        account: Some(&scope),
        ..ctx()
    };
    let account = resolve_quote(&offer, &account_ctx, qty(100));
    let account_quote = resolved(&account);
    assert_eq!(account_quote.merchandise, money("1700.00"));
    assert!(account.notes.contains(&QuoteNote::AccountPriceApplied));

    let other_scope = AccountScope::new("acct-2").expect("scope");
    let other_ctx = PricingContext {
        account: Some(&other_scope),
        ..ctx()
    };
    let other = resolve_quote(&offer, &other_ctx, qty(100));
    assert_eq!(resolved(&other).merchandise, money("2000.00"));
}

#[test]
fn duplicate_minima_remain_refused_within_one_visibility_scope() {
    // Two PUBLIC tiers at the same minimum stay a duplicate...
    let duplicate = with_tiers(base_offer(), vec![tier(100, "20.00"), tier(100, "19.00")]);
    let json = serde_json::to_value(&duplicate).expect("serialize");
    assert!(serde_json::from_value::<CommercialOffer>(json).is_err());

    // ...and two ACCOUNT tiers of the SAME scope at the same minimum would
    // be ambiguous for that account: also refused.
    let scope = AccountScope::new("acct-1").expect("scope");
    let mut first = tier(100, "17.00");
    first.visibility = PriceVisibility::AccountSpecific;
    first.account_scope = Some(scope.clone());
    let mut second = tier(100, "16.00");
    second.visibility = PriceVisibility::AccountSpecific;
    second.account_scope = Some(scope);
    let ambiguous = with_tiers(base_offer(), vec![first, second]);
    let json = serde_json::to_value(&ambiguous).expect("serialize");
    assert!(serde_json::from_value::<CommercialOffer>(json).is_err());
}

#[test]
fn inquiry_required_never_fabricates_a_price() {
    let mut offer = base_offer();
    offer.price_visibility = PriceVisibility::InquiryRequired;
    offer.price_breaks = Vec::new();
    offer.moq = Some(nz(1));
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(1000));
    assert_eq!(resolution.status, QuoteStatus::InquiryRequired);
    assert!(resolution.quote.is_none());
    assert!(resolution.unit_price.is_none());
    assert!(resolution.price_range.is_none());
    assert!(resolution.billed_quantity.is_none());
}

#[test]
fn no_price_and_unknown_visibility_are_not_zero_prices() {
    let offer = base_offer();
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(10));
    assert_eq!(resolution.status, QuoteStatus::NoPrice);
    assert!(resolution.quote.is_none());

    let mut unknown = base_offer();
    unknown.price_visibility = PriceVisibility::Unknown;
    let resolution = price_at_quantity(&unknown, VariantRequest::None, qty(10));
    assert_eq!(resolution.status, QuoteStatus::NoPrice);
    assert!(resolution.quote.is_none());
}

#[test]
fn quote_unknown_components_are_none_never_zero() {
    let offer = spec_example_offer();
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(5000));
    let quote = resolved(&resolution);
    assert!(quote.shipping.is_none());
    assert!(quote.tax.is_none());
    assert!(quote.duty.is_none());
    assert!(!quote.is_complete());
    assert!(quote.total_is_lower_bound());
    assert_eq!(quote.total, quote.merchandise);

    let shipping_ctx = PricingContext {
        shipping: Some(money("123.45")),
        ..ctx()
    };
    let with_shipping = resolve_quote(&offer, &shipping_ctx, qty(5000));
    let quote = resolved(&with_shipping);
    assert_eq!(quote.shipping.expect("shipping"), money("123.45"));
    assert!(quote.tax.is_none());
    assert_eq!(quote.total, money("91123.45"));
    assert!(
        quote.total_is_lower_bound(),
        "an unknown tax still makes the total a lower bound"
    );

    let full_ctx = PricingContext {
        shipping: Some(money("123.45")),
        tax: Some(money("10.00")),
        duty: Some(money("5.00")),
        ..ctx()
    };
    let complete = resolve_quote(&offer, &full_ctx, qty(5000));
    let quote = resolved(&complete);
    assert!(quote.is_complete());
    assert!(!quote.total_is_lower_bound());
    assert_eq!(quote.total, money("91138.45"));
}

#[test]
fn landed_cost_currency_mismatch_is_typed_not_converted() {
    let offer = spec_example_offer();
    let mismatched = PricingContext {
        shipping: Some(usd("10.00")),
        ..ctx()
    };
    let resolution = resolve_quote(&offer, &mismatched, qty(5000));
    assert_eq!(resolution.status, QuoteStatus::CurrencyMismatch);
    assert!(resolution.quote.is_none());
}

#[test]
fn amount_overflow_is_typed_not_wrapped() {
    let mut offer = with_tiers(base_offer(), vec![tier(1, "9223372036854.775807")]);
    offer.moq = Some(nz(1));
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(1_000_000_000));
    assert_eq!(resolution.status, QuoteStatus::AmountOverflow);
    assert!(resolution.quote.is_none());
}

#[test]
fn unsatisfiable_quantity_step_is_typed() {
    let mut offer = with_tiers(base_offer(), vec![tier(1, "10.00")]);
    offer.order_multiple = Some(nz(999_999_937));
    offer.standard_pack = Some(nz(999_999_929));
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(1));
    assert_eq!(resolution.status, QuoteStatus::QuantityStepUnsatisfiable);
    assert!(resolution.quote.is_none());
}

#[test]
fn quantity_above_the_maximum_rounds_out_of_range() {
    let mut offer = with_tiers(base_offer(), vec![tier(1, "1.00")]);
    offer.order_multiple = Some(nz(7));
    let resolution = price_at_quantity(
        &offer,
        VariantRequest::None,
        Quantity::new(1_000_000_000).expect("qty"),
    );
    assert_eq!(resolution.status, QuoteStatus::QuantityTooLarge);
    assert!(resolution.quote.is_none());
}

#[test]
fn resolution_serde_round_trips_and_rejects_inconsistent_documents() {
    let offer = spec_example_offer();
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(5000));
    let json = serde_json::to_string(&resolution).expect("serialize");
    let back: QuoteResolution = serde_json::from_str(&json).expect("round trip");
    assert_eq!(back, resolution);
    assert!(json.contains("\"status\":\"resolved\""));
    assert!(json.contains("\"amount\":\"18.200000\""), "{json}");
    assert!(json.contains("\"amount\":\"91000.000000\""), "{json}");

    let ambiguous = {
        let mut offer = base_offer();
        offer.variants = vec![
            variant("v1", None, vec![tier(1, "2.00")]),
            variant("v2", None, vec![tier(1, "36.00")]),
        ];
        price_at_quantity(&offer, VariantRequest::None, qty(10))
    };
    let json = serde_json::to_string(&ambiguous).expect("serialize");
    assert!(json.contains("\"status\":\"variant_ambiguous\""));
    let back: QuoteResolution = serde_json::from_str(&json).expect("round trip");
    assert_eq!(back, ambiguous);

    let resolved_json = serde_json::to_value(&resolution).expect("value");
    let mut broken = resolved_json.clone();
    broken["quote"] = serde_json::Value::Null;
    assert!(
        serde_json::from_value::<QuoteResolution>(broken).is_err(),
        "resolved without a quote must be rejected"
    );

    let mut fabricated = serde_json::to_value(&ambiguous).expect("value");
    fabricated["quote"] = resolved_json["quote"].clone();
    assert!(
        serde_json::from_value::<QuoteResolution>(fabricated).is_err(),
        "a non-resolved status must never carry a quote"
    );

    let mut no_candidates = serde_json::to_value(&ambiguous).expect("value");
    no_candidates["candidates"] = serde_json::json!([]);
    assert!(serde_json::from_value::<QuoteResolution>(no_candidates).is_err());

    assert!(serde_json::from_str::<QuoteResolution>(r#"{"status":"teleported"}"#).is_err());
    assert!(serde_json::from_str::<QuoteResolution>(
        r#"{"status":"moq_violation","requested_quantity":5}"#
    )
    .is_err());
}

#[test]
fn quote_serde_is_exact_and_rejects_inconsistent_totals() {
    let quote = Quote::new(money("100.00"), None, Some(money("7.00")), None).expect("quote");
    assert_eq!(quote.total, money("107.00"));
    let json = serde_json::to_value(&quote).expect("serialize");
    assert_eq!(json["shipping"], serde_json::Value::Null);
    assert_eq!(json["total"]["amount"], "107.000000");
    assert_eq!(
        serde_json::from_value::<Quote>(json.clone()).expect("round trip"),
        quote
    );

    let mut broken = json.clone();
    broken["total"]["amount"] = serde_json::json!("108.000000");
    assert!(serde_json::from_value::<Quote>(broken).is_err());

    let mut mismatched = json;
    mismatched["tax"]["currency"] = serde_json::json!("USD");
    assert!(serde_json::from_value::<Quote>(mismatched).is_err());

    assert!(serde_json::from_str::<Quote>(
        r#"{"merchandise":{"currency":"CNY","amount":"1.000000"},"shipping":{"currency":"CNY","amount":"0.000000"},"tax":null,"duty":null,"total":{"currency":"CNY","amount":"1.000000"},"extra":1}"#
    )
    .is_err());
}

#[test]
fn offer_validation_rejects_overlapping_and_duplicate_tiers() {
    let mut overlapping = with_tiers(
        base_offer(),
        vec![tier_range(100, 2000, "20.00"), tier(1000, "19.00")],
    );
    overlapping.moq = Some(nz(100));
    let json = serde_json::to_value(&overlapping).expect("serialize");
    assert!(serde_json::from_value::<CommercialOffer>(json).is_err());

    let duplicate = with_tiers(base_offer(), vec![tier(100, "20.00"), tier(100, "19.00")]);
    let json = serde_json::to_value(&duplicate).expect("serialize");
    assert!(serde_json::from_value::<CommercialOffer>(json).is_err());
}

#[test]
fn offer_validation_rejects_fabricated_inquiry_prices_and_mixed_currencies() {
    let mut inquiry = base_offer();
    inquiry.price_visibility = PriceVisibility::InquiryRequired;
    inquiry.price_breaks = vec![tier(1, "1.00")];
    let json = serde_json::to_value(&inquiry).expect("serialize");
    assert!(serde_json::from_value::<CommercialOffer>(json).is_err());

    let mut mixed = with_tiers(base_offer(), vec![tier(1, "1.00")]);
    mixed.variants = vec![variant("v1", None, vec![tier(1, "2.00")])];
    mixed.variants[0].price_breaks[0].unit_price = usd("2.00");
    let json = serde_json::to_value(&mixed).expect("serialize");
    assert!(serde_json::from_value::<CommercialOffer>(json).is_err());
}

#[test]
fn offer_serde_round_trips_and_is_strict_about_unknown_fields() {
    let mut offer = spec_example_offer();
    offer.variants = vec![variant(
        "v1",
        Some(PackagingType::Tray),
        vec![tier(100, "20.00")],
    )];
    offer.packaging = vec![packaging_option(
        PackagingType::CutTape,
        vec![tier(100, "20.00")],
    )];
    offer.moq = Some(nz(100));
    offer.lifecycle = LifecycleStatus::Active;
    let json = serde_json::to_string(&offer).expect("serialize");
    let back: CommercialOffer = serde_json::from_str(&json).expect("round trip");
    assert_eq!(back, offer);

    let mut value = serde_json::to_value(&offer).expect("value");
    value["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<CommercialOffer>(value).is_err());

    let mut value = serde_json::to_value(&offer).expect("value");
    value["price_breaks"][0]["unit_price"]["amount"] = serde_json::json!(20.0);
    assert!(serde_json::from_value::<CommercialOffer>(value).is_err());

    let mut value = serde_json::to_value(&offer).expect("value");
    value["variants"][0]["variant_id"] = serde_json::json!("");
    assert!(serde_json::from_value::<CommercialOffer>(value).is_err());
}

#[test]
fn hostile_offer_bounds_are_enforced() {
    let mut too_many = spec_example_offer();
    too_many.variants = (0..(MAX_VARIANTS_PER_OFFER + 1))
        .map(|index| variant(&format!("v{index}"), None, vec![tier(1, "1.00")]))
        .collect();
    assert!(matches!(
        too_many.validate(),
        Err(OfferError::TooManyVariants { .. })
    ));

    let mut too_many_tiers = base_offer();
    too_many_tiers.price_breaks = (0..(MAX_PRICE_BREAKS_PER_LIST + 1))
        .map(|index| tier(index as u64 + 1, "1.00"))
        .collect();
    assert!(matches!(
        too_many_tiers.validate(),
        Err(OfferError::TooManyPriceBreaks { .. })
    ));

    let mut too_many_attributes = base_offer();
    let mut v = variant("v1", None, Vec::new());
    v.attributes = (0..(MAX_ATTRIBUTES_PER_VARIANT + 1))
        .map(|index| attribute(&format!("a{index}"), "x"))
        .collect();
    too_many_attributes.variants = vec![v];
    assert!(matches!(
        too_many_attributes.validate(),
        Err(OfferError::TooManyAttributes { .. })
    ));

    let mut too_many_packaging = base_offer();
    too_many_packaging.packaging = (0..(MAX_PACKAGING_OPTIONS + 1))
        .map(|_| packaging_option(PackagingType::Tray, Vec::new()))
        .collect();
    assert!(matches!(
        too_many_packaging.validate(),
        Err(OfferError::TooManyPackagingOptions { .. })
    ));
}

#[test]
fn duplicate_variant_ids_and_attributes_are_rejected() {
    let mut duplicate_ids = base_offer();
    duplicate_ids.variants = vec![
        variant("v1", None, vec![tier(1, "1.00")]),
        variant("v1", None, vec![tier(1, "2.00")]),
    ];
    assert!(matches!(
        duplicate_ids.validate(),
        Err(OfferError::DuplicateVariantId { .. })
    ));

    let mut duplicate_attributes = base_offer();
    let mut v = variant("v1", None, vec![tier(1, "1.00")]);
    v.attributes = vec![attribute("grade", "a"), attribute("GRADE ", "b")];
    duplicate_attributes.variants = vec![v];
    assert!(matches!(
        duplicate_attributes.validate(),
        Err(OfferError::DuplicateAttribute { .. })
    ));
}

#[test]
fn truncated_and_byte_mutated_documents_never_panic() {
    let offer = spec_example_offer();
    let json = serde_json::to_string(&offer).expect("serialize");
    for end in 0..json.len() {
        if let Some(slice) = json.get(..end) {
            let _ = serde_json::from_str::<CommercialOffer>(slice);
        }
    }
    let bytes = json.as_bytes();
    for index in 0..bytes.len() {
        let mut mutated = bytes.to_vec();
        mutated[index] = b'\x00';
        if let Ok(text) = std::str::from_utf8(&mutated) {
            let _ = serde_json::from_str::<CommercialOffer>(text);
        }
    }
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(5000));
    let json = serde_json::to_string(&resolution).expect("serialize");
    for end in 0..json.len() {
        if let Some(slice) = json.get(..end) {
            let _ = serde_json::from_str::<QuoteResolution>(slice);
        }
    }
    let money = Money::from_micros(Currency::CNY, i64::MAX);
    let json = serde_json::to_string(&money).expect("serialize");
    for end in 0..json.len() {
        if let Some(slice) = json.get(..end) {
            let _ = serde_json::from_str::<Money>(slice);
        }
    }
}

#[test]
fn stock_evidence_notes_do_not_fabricate_availability() {
    let mut offer = spec_example_offer();
    offer.stock = StockState::InStock { quantity: nz(10) };
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(5000));
    let quote = resolved(&resolution);
    assert_eq!(quote.merchandise, money("91000.00"));
    assert!(resolution
        .notes
        .contains(&QuoteNote::StockBelowBilledQuantity));

    offer.stock = StockState::OutOfStock;
    let resolution = price_at_quantity(&offer, VariantRequest::None, qty(5000));
    assert_eq!(
        resolution.status,
        QuoteStatus::Resolved,
        "out of stock is a stock fact, not a pricing failure"
    );
    assert!(!resolution
        .notes
        .contains(&QuoteNote::StockBelowBilledQuantity));
}
