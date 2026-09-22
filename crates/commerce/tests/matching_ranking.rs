//! Adversarial tests for matching certainty and deterministic ranking.

mod common;

use common::*;
use faktor_commerce::*;

fn query(mpn: &str, manufacturer: Option<&str>) -> PartQuery {
    PartQuery {
        manufacturer: manufacturer.map(text),
        part_number: text(mpn),
        package: None,
        packaging: None,
        quantity: None,
        attributes: Vec::new(),
    }
}

#[test]
fn exact_mpn_and_manufacturer_is_exact() {
    let offer = base_offer();
    let result = match_offer(
        &query("STM32F407VGT6", Some("STMicroelectronics")),
        &offer,
        0,
    );
    assert_eq!(result.certainty, MatchCertainty::Exact);
    assert!(result.signals.mpn_exact);
    assert!(result.signals.manufacturer_exact);
    assert!(result.signals.manufacturer_known);
    assert!(!result.signals.suffix_difference);
    assert_eq!(result.matched_part_number.as_deref(), Some("STM32F407VGT6"));
    assert_eq!(
        result.matched_manufacturer.as_deref(),
        Some("STMicroelectronics")
    );
}

#[test]
fn case_and_space_insensitive_mpn_is_exact() {
    let offer = base_offer();
    for spelling in ["stm32f407vgt6", "STM32 F407 VGT6", " Stm32F407vgt6 "] {
        let result = match_offer(&query(spelling, Some("STMicroelectronics")), &offer, 0);
        assert_eq!(result.certainty, MatchCertainty::Exact, "{spelling}");
        assert!(result.signals.mpn_exact);
    }
}

#[test]
fn packaging_suffix_is_strong_with_machine_readable_signals() {
    let mut offer = base_offer();
    offer.identity.manufacturer_part_number = Some(text("STM32F407VGT6TR"));
    let result = match_offer(
        &query("STM32F407VGT6", Some("STMicroelectronics")),
        &offer,
        0,
    );
    assert_eq!(result.certainty, MatchCertainty::Strong);
    assert!(!result.signals.mpn_exact);
    assert!(result.signals.mpn_base_equal);
    assert!(result.signals.suffix_difference);
    assert_eq!(
        result.signals.suffix_kinds,
        vec![SuffixKind::Packaging(PackagingType::TapeAndReel)]
    );

    let exact_reel = match_offer(
        &query("STM32F407VGT6TR", Some("STMicroelectronics")),
        &offer,
        0,
    );
    assert_eq!(exact_reel.certainty, MatchCertainty::Exact);
    assert!(!exact_reel.signals.suffix_difference);
}

#[test]
fn non_packaging_suffix_difference_is_only_possible() {
    let mut offer = base_offer();
    offer.identity.manufacturer_part_number = Some(text("MCU-85C"));
    let result = match_offer(&query("MCU-125C", Some("STMicroelectronics")), &offer, 0);
    assert_eq!(result.certainty, MatchCertainty::Possible);
    assert!(result.signals.mpn_base_equal);
    assert_eq!(
        result.signals.suffix_kinds,
        vec![SuffixKind::TemperatureGrade]
    );
}

#[test]
fn a_contradicted_manufacturer_is_a_mismatch_even_with_an_exact_mpn() {
    let offer = base_offer();
    let result = match_offer(
        &query("STM32F407VGT6", Some("Texas Instruments")),
        &offer,
        0,
    );
    assert_eq!(result.certainty, MatchCertainty::Mismatch);
    assert!(result.signals.mpn_exact);
    assert!(result.signals.manufacturer_known);
    assert!(!result.signals.manufacturer_exact);
    assert!(result.signals.manufacturer_conflict);
}

#[test]
fn a_different_part_number_is_a_mismatch() {
    let offer = base_offer();
    let result = match_offer(
        &query("STM32F103C8T6", Some("STMicroelectronics")),
        &offer,
        0,
    );
    assert_eq!(result.certainty, MatchCertainty::Mismatch);
    assert!(!result.signals.mpn_exact);
    assert!(!result.signals.mpn_base_equal);
}

#[test]
fn exact_mpn_without_a_stated_manufacturer_is_strong_not_exact() {
    let offer = base_offer();
    let result = match_offer(&query("STM32F407VGT6", None), &offer, 0);
    assert_eq!(result.certainty, MatchCertainty::Strong);
    assert!(result.signals.mpn_exact);
    assert!(!result.signals.manufacturer_exact);
    assert!(
        !result.signals.manufacturer_conflict,
        "an unstated query manufacturer is not a contradiction"
    );
}

#[test]
fn source_part_number_only_is_possible_never_exact() {
    let mut offer = base_offer();
    offer.identity.manufacturer_part_number = None;
    offer.identity.source_part_number = Some(text("C1234"));
    let result = match_offer(&query("c1234", None), &offer, 0);
    assert_eq!(result.certainty, MatchCertainty::Possible);
    assert!(result.signals.source_part_exact);
    assert!(!result.signals.mpn_exact);
    assert_eq!(result.matched_part_number.as_deref(), Some("C1234"));
}

#[test]
fn title_overlap_can_only_ever_be_possible() {
    let mut offer = base_offer();
    offer.identity.manufacturer_part_number = None;
    offer.identity.source_part_number = None;
    offer.title = text("STM32F407VGT6 LQFP100 microcontroller");
    let result = match_offer(&query("STM32F407VGT6", None), &offer, 0);
    assert_eq!(result.certainty, MatchCertainty::Possible);
    assert_eq!(result.signals.title_token_overlap_bp, Some(10_000));

    offer.title = text("unrelated resistor kit");
    let result = match_offer(&query("STM32F407VGT6", None), &offer, 0);
    assert_eq!(result.certainty, MatchCertainty::Mismatch);
}

#[test]
fn package_and_packaging_signals_are_three_valued() {
    let mut offer = base_offer();
    let mut v = variant(
        "v1",
        Some(PackagingType::TapeAndReel),
        vec![tier(100, "20.00")],
    );
    v.attributes = vec![attribute("package", "LQFP-100")];
    offer.variants = vec![v];

    let mut q = query("STM32F407VGT6", Some("STMicroelectronics"));
    q.package = Some(text("lqfp100"));
    q.packaging = Some(PackagingType::TapeAndReel);
    let result = match_offer(&q, &offer, 0);
    assert_eq!(result.signals.package_match, Some(true));
    assert_eq!(result.signals.packaging_match, Some(true));

    q.package = Some(text("TQFP-44"));
    let result = match_offer(&q, &offer, 0);
    assert_eq!(result.signals.package_match, Some(false));

    q.package = None;
    q.packaging = Some(PackagingType::Tray);
    let result = match_offer(&q, &offer, 0);
    assert_eq!(result.signals.package_match, None);
    assert_eq!(result.signals.packaging_match, Some(false));

    q.packaging = None;
    let result = match_offer(&q, &offer, 0);
    assert_eq!(result.signals.packaging_match, None);
}

#[test]
fn set_level_ambiguity_is_never_auto_resolved() {
    let mut first = base_offer();
    first.source = source("lcsc");
    let mut second = base_offer();
    second.source = source("mouser");
    second.identity.offer_id = Some(text("offer-2"));

    let outcome = match_offers(
        &query("STM32F407VGT6", Some("STMicroelectronics")),
        &[first.clone(), second],
    );
    assert_eq!(outcome.certainty, MatchCertainty::Ambiguous);
    assert_eq!(outcome.matches.len(), 2);
    assert_eq!(outcome.mismatches, 0);

    let mut third = base_offer();
    third.identity.manufacturer_part_number = Some(text("STM32F407VGT6TR"));
    let outcome = match_offers(
        &query("STM32F407VGT6", Some("STMicroelectronics")),
        &[first, third],
    );
    assert_eq!(
        outcome.certainty,
        MatchCertainty::Exact,
        "one exact candidate is unambiguous"
    );
    assert_eq!(outcome.matches.len(), 2);
}

#[test]
fn match_outcome_counts_mismatches() {
    let mut mismatched = base_offer();
    mismatched.identity.manufacturer_part_number = Some(text("NE555P"));
    let outcome = match_offers(
        &query("STM32F407VGT6", Some("STMicroelectronics")),
        &[base_offer(), mismatched],
    );
    assert_eq!(outcome.certainty, MatchCertainty::Exact);
    assert_eq!(outcome.matches.len(), 1);
    assert_eq!(outcome.mismatches, 1);

    let outcome = match_offers(&query("NOPE-1", None), &[base_offer()]);
    assert_eq!(outcome.certainty, MatchCertainty::Mismatch);
    assert!(outcome.matches.is_empty());
}

#[test]
fn signals_are_machine_readable_never_a_bare_score() {
    let result = match_offer(
        &query("STM32F407VGT6", Some("STMicroelectronics")),
        &base_offer(),
        0,
    );
    let json = serde_json::to_value(&result).expect("serialize");
    assert_eq!(json["certainty"], "exact");
    assert_eq!(json["signals"]["mpn_exact"], true);
    assert_eq!(json["signals"]["manufacturer_exact"], true);
    assert_eq!(json["signals"]["package_match"], serde_json::Value::Null);
    assert_eq!(json["signals"]["suffix_difference"], false);
    assert!(json["signals"].get("score").is_none());
    assert!(json.get("score").is_none());
    let back: OfferMatch = serde_json::from_value(json).expect("round trip");
    assert_eq!(back, result);
}

#[test]
fn part_query_is_bounded_and_strict() {
    assert!(PartQuery::new("STM32F407VGT6").is_ok());
    assert!(PartQuery::new("").is_err());
    assert!(PartQuery::new("A\u{202e}B").is_err());
    let long = "A".repeat(MAX_PART_NUMBER_BYTES + 1);
    assert!(PartQuery::new(&long).is_err());

    let mut too_many = PartQuery::new("STM32F407VGT6").expect("query");
    too_many.attributes = (0..(MAX_ATTRIBUTES_PER_VARIANT + 1))
        .map(|index| attribute(&format!("a{index}"), "x"))
        .collect();
    assert!(too_many.validate().is_err());

    assert!(serde_json::from_str::<PartQuery>(
        r#"{"manufacturer":null,"part_number":"AB","package":null,"packaging":null,"quantity":5,"attributes":[],"extra":1}"#
    )
    .is_err());
    assert!(serde_json::from_str::<PartQuery>(
        r#"{"manufacturer":null,"part_number":"AB","package":null,"packaging":null,"quantity":-5,"attributes":[]}"#
    )
    .is_err());
}

fn ranked_identity_order(offers: &[CommercialOffer], quantity: u64) -> Vec<String> {
    let q = query("STM32F407VGT6", Some("STMicroelectronics"));
    let ctx = RankingContext::at_quantity(qty(quantity));
    rank_offers(&q, offers, &ctx)
        .into_iter()
        .map(|ranked| {
            ranked
                .offer
                .identity
                .offer_id
                .as_ref()
                .map(|id| id.as_str().to_string())
                .unwrap_or_default()
        })
        .collect()
}

fn distinct_offer(offer_id: &str, source_id: &str, mpn: &str, title: &str) -> CommercialOffer {
    let mut offer = base_offer();
    offer.source = source(source_id);
    offer.identity = identity(mpn, "STMicroelectronics");
    offer.identity.offer_id = Some(text(offer_id));
    offer.title = text(title);
    offer.provenance.source = source(source_id);
    offer
}

#[test]
fn ranking_is_stable_under_input_permutation() {
    let a = {
        let mut offer = distinct_offer("offer-a", "lcsc", "STM32F407VGT6", "STM32F407VGT6 reel");
        offer.price_breaks = vec![tier(100, "20.00")];
        offer.stock = StockState::Unknown;
        offer.lifecycle = LifecycleStatus::Unknown;
        offer
    };
    let b = {
        let mut offer = distinct_offer(
            "offer-b",
            "mouser",
            "STM32F407VGT6",
            "STM32F407VGT6 LQFP100 in stock",
        );
        offer.price_breaks = vec![tier(100, "18.00")];
        offer
    };
    let c = {
        let mut offer = distinct_offer("offer-c", "digikey", "STM32F407VGT6TR", "STM32F407VGT6TR");
        offer.price_breaks = vec![tier(100, "19.00")];
        offer
    };
    let d = {
        let mut offer = distinct_offer("offer-d", "alibaba", "NE555P", "timer ic cheap");
        offer.price_breaks = vec![tier(1, "0.10")];
        offer
    };

    let canonical = ranked_identity_order(&[a.clone(), b.clone(), c.clone(), d.clone()], 500);
    assert_eq!(canonical.len(), 4);
    assert_eq!(canonical[3], "offer-d", "the mismatching offer ranks last");
    assert_eq!(
        canonical[0], "offer-b",
        "the best exact+in-stock offer wins"
    );

    let permutations = [
        vec![d.clone(), c.clone(), b.clone(), a.clone()],
        vec![b.clone(), a.clone(), d.clone(), c.clone()],
        vec![c.clone(), d.clone(), a.clone(), b.clone()],
        vec![a.clone(), d.clone(), b.clone(), c.clone()],
    ];
    for permutation in permutations {
        assert_eq!(
            ranked_identity_order(&permutation, 500),
            canonical,
            "ranking must not depend on input order"
        );
    }
}

#[test]
fn ranking_scores_are_transparent_integer_components() {
    let q = query("STM32F407VGT6", Some("STMicroelectronics"));
    let ctx = RankingContext::at_quantity(qty(500));
    let offers = [base_offer()];
    let ranked = rank_offers(&q, &offers, &ctx);
    let score = &ranked[0].score;
    let expected_keys = [
        ScoreKey::ExactMpn,
        ScoreKey::Manufacturer,
        ScoreKey::Package,
        ScoreKey::QuantityCompatibility,
        ScoreKey::Stock,
        ScoreKey::Moq,
        ScoreKey::OrderMultiple,
        ScoreKey::Lifecycle,
        ScoreKey::PriceCompleteness,
        ScoreKey::SourceConfidence,
        ScoreKey::SellerCompleteness,
        ScoreKey::TitleTokenOverlap,
        ScoreKey::VariantOverlap,
        ScoreKey::SupplierEvidence,
    ];
    assert_eq!(score.components.len(), expected_keys.len());
    let mut total = 0i64;
    for (component, key) in score.components.iter().zip(expected_keys) {
        assert_eq!(component.key, key);
        assert_eq!(component.weight, key.weight());
        assert!((0..=1_000).contains(&component.normalized));
        assert_eq!(
            component.contribution,
            component.weight * component.normalized / 1_000
        );
        total += component.contribution;
    }
    assert_eq!(score.total, total);
    assert!(score.total > 0);

    let json = serde_json::to_value(score).expect("serialize");
    assert!(json["total"].is_i64());
    assert_eq!(json["components"].as_array().expect("array").len(), 14);
}

#[test]
fn ranking_prefers_evidence_over_absence() {
    let exact = {
        let mut offer = distinct_offer("exact", "lcsc", "STM32F407VGT6", "STM32F407VGT6");
        offer.price_breaks = vec![tier(100, "20.00")];
        offer.lifecycle = LifecycleStatus::Active;
        offer
    };
    let mut weak = distinct_offer("weak", "alibaba", "OTHER", "STM32F407VGT6 compatible");
    weak.price_breaks = vec![tier(1, "1.00")];
    weak.lifecycle = LifecycleStatus::Obsolete;

    let order = ranked_identity_order(&[weak, exact], 500);
    assert_eq!(order[0], "exact");
}

#[test]
fn equal_scores_tie_break_deterministically() {
    let mut mouser = base_offer();
    mouser.source = source("mouser");
    mouser.provenance.source = source("mouser");
    let mut lcsc = base_offer();
    lcsc.source = source("lcsc");
    lcsc.provenance.source = source("lcsc");
    let order = ranked_identity_order(&[mouser.clone(), lcsc.clone()], 500);
    let q = query("STM32F407VGT6", Some("STMicroelectronics"));
    let ctx = RankingContext::at_quantity(qty(500));
    let scores: Vec<i64> = rank_offers(&q, &[mouser.clone(), lcsc.clone()], &ctx)
        .iter()
        .map(|ranked| ranked.score.total)
        .collect();
    if scores[0] == scores[1] {
        assert_eq!(order, vec!["offer-1".to_string(), "offer-1".to_string()]);
    }
    // Both offers carry the same offer id here; the stable order must be the
    // source tie-break: lcsc before mouser.
    let ordered_sources: Vec<String> = rank_offers(&q, &[mouser, lcsc], &ctx)
        .iter()
        .map(|ranked| ranked.offer.source.as_str().to_string())
        .collect();
    assert_eq!(ordered_sources, vec!["lcsc", "mouser"]);
}

#[test]
fn ranking_uses_the_account_and_packaging_context() {
    let mut offer = with_tiers(base_offer(), vec![tier(1000, "20.00")]);
    let mut account_tier = tier(10, "18.00");
    account_tier.visibility = PriceVisibility::AccountSpecific;
    account_tier.account_scope = Some(AccountScope::new("acct-1").expect("scope"));
    offer.price_breaks.push(account_tier);
    let q = query("STM32F407VGT6", Some("STMicroelectronics"));

    let anonymous_ctx = RankingContext::at_quantity(qty(100));
    let anonymous = rank_offers(&q, std::slice::from_ref(&offer), &anonymous_ctx);
    assert_eq!(
        anonymous[0]
            .score
            .components
            .iter()
            .find(|component| component.key == ScoreKey::QuantityCompatibility)
            .expect("component")
            .normalized,
        0,
        "an anonymous caller cannot use account pricing"
    );

    let account_ctx = RankingContext {
        account: Some(AccountScope::new("acct-1").expect("scope")),
        ..RankingContext::at_quantity(qty(100))
    };
    let offers = [offer];
    let with_account = rank_offers(&q, &offers, &account_ctx);
    assert_eq!(
        with_account[0]
            .score
            .components
            .iter()
            .find(|component| component.key == ScoreKey::QuantityCompatibility)
            .expect("component")
            .normalized,
        1_000
    );
}

#[test]
fn ranking_ignores_promotion_expiry_noise_by_default() {
    let mut offer = with_tiers(base_offer(), vec![tier(100, "20.00")]);
    offer.price_breaks[0].promotion = Some(Promotion {
        id: text("promo"),
        kind: PromotionKind::PercentOff {
            basis_points: BasisPoints::new(9_000).expect("bp"),
        },
        valid_until_ms: None,
        label: None,
    });
    let q = query("STM32F407VGT6", Some("STMicroelectronics"));
    let ctx = RankingContext::at_quantity(qty(500));
    let offers = [offer];
    let ranked = rank_offers(&q, &offers, &ctx);
    assert_eq!(
        ranked[0]
            .score
            .components
            .iter()
            .find(|component| component.key == ScoreKey::PriceCompleteness)
            .expect("component")
            .normalized,
        1_000
    );
}
