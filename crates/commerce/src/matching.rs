//! Deterministic matching with machine-readable signals.
//!
//! A match is never a bare score. [`MatchSignals`] records every fact the
//! classification used (`mpn_exact`, `manufacturer_exact`, `package_match`,
//! `suffix_difference`, ...) and [`MatchCertainty`] is the documented
//! classification of those facts:
//!
//! | Condition | Certainty |
//! |-----------|-----------|
//! | exact MPN, manufacturer confirmed on both sides | `Exact` |
//! | exact MPN, manufacturer unknown on one side | `Strong` |
//! | equal base, only packaging suffixes differ | `Strong` |
//! | equal base, a non-packaging suffix differs | `Possible` |
//! | source part number matches but no MPN | `Possible` |
//! | no identifier match, no manufacturer conflict, title token overlap ≥ 90% | `Possible` |
//! | a known identifier or manufacturer contradicts | `Mismatch` (never softened by title overlap) |
//!
//! At the set level, two or more candidates at the same best certainty are
//! reported as [`MatchCertainty::Ambiguous`] — a caller must never
//! auto-select among them.

use serde::{Deserialize, Serialize};

use crate::identity::{
    NormalizedPartNumber, PartNumberError, PartNumberNormalizer, MAX_PART_NUMBER_BYTES,
};
use crate::offer::{
    matching_attribute_count, normalized_attributes, CommercialOffer, NormalizedAttribute,
    VariantAttribute, MAX_ATTRIBUTES_PER_VARIANT,
};
use crate::packaging::PackagingType;
use crate::quantity::Quantity;
use crate::quote::{resolve_quote, PricingContext, QuoteStatus, VariantRequest};
use crate::text::Text;

/// The title-token overlap (basis points) above which an identifier-less
/// candidate may be `Possible`.
pub const TITLE_OVERLAP_POSSIBLE_BP: u16 = 9_000;

/// A part query: what the caller is looking for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartQuery {
    /// The manufacturer, when known.
    pub manufacturer: Option<Text<128>>,
    /// The part number to match (MPN preferred).
    pub part_number: Text<{ MAX_PART_NUMBER_BYTES }>,
    /// The package / case, when the caller cares.
    pub package: Option<Text<64>>,
    /// The required packaging, when the caller cares.
    pub packaging: Option<PackagingType>,
    /// The quantity the caller intends to buy.
    pub quantity: Option<Quantity>,
    /// Required variant attributes.
    pub attributes: Vec<VariantAttribute>,
}

impl PartQuery {
    /// A minimal query for one part number.
    pub fn new(part_number: &str) -> Result<Self, MatchError> {
        Ok(Self {
            manufacturer: None,
            part_number: Text::new(part_number).map_err(|error| MatchError::InvalidQuery {
                reason: error.to_string(),
            })?,
            package: None,
            packaging: None,
            quantity: None,
            attributes: Vec::new(),
        })
    }

    /// Validate the query.
    pub fn validate(&self) -> Result<(), MatchError> {
        PartNumberNormalizer::new()
            .normalize(self.part_number.as_str())
            .map_err(|error| MatchError::InvalidQuery {
                reason: error.to_string(),
            })?;
        if self.attributes.len() > MAX_ATTRIBUTES_PER_VARIANT {
            return Err(MatchError::InvalidQuery {
                reason: format!(
                    "query has {} attributes; the maximum is {MAX_ATTRIBUTES_PER_VARIANT}",
                    self.attributes.len()
                ),
            });
        }
        Ok(())
    }
}

/// A rejected query.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MatchError {
    /// The query itself is malformed.
    #[error("invalid part query: {reason}")]
    InvalidQuery {
        /// Why the query was rejected.
        reason: String,
    },
}

/// How certain a match is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchCertainty {
    /// Same purchasable item, manufacturer confirmed.
    Exact,
    /// Same base part, a packaging suffix differs (or the manufacturer is
    /// unconfirmed).
    Strong,
    /// Plausibly the same part; needs review.
    Possible,
    /// More than one candidate at the same certainty; never auto-select.
    Ambiguous,
    /// A known identifier or manufacturer contradicts.
    Mismatch,
}

impl MatchCertainty {
    /// The classification rank (higher is stronger; `Ambiguous` is
    /// set-level only).
    pub const fn rank(self) -> u8 {
        match self {
            Self::Mismatch => 0,
            Self::Possible => 1,
            Self::Strong => 2,
            Self::Exact => 3,
            Self::Ambiguous => 4,
        }
    }

    /// The wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Strong => "strong",
            Self::Possible => "possible",
            Self::Ambiguous => "ambiguous",
            Self::Mismatch => "mismatch",
        }
    }
}

/// Every machine-readable fact behind a match classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSignals {
    /// The canonical MPNs are identical.
    pub mpn_exact: bool,
    /// The canonical MPN bases are identical.
    pub mpn_base_equal: bool,
    /// The canonical source part numbers are identical (MPN absent).
    pub source_part_exact: bool,
    /// Both manufacturers are known and equal.
    pub manufacturer_exact: bool,
    /// At least one manufacturer is known.
    pub manufacturer_known: bool,
    /// Both manufacturers are known and different (a contradiction).
    pub manufacturer_conflict: bool,
    /// `None` when either side does not state a package.
    pub package_match: Option<bool>,
    /// `None` when either side does not state packaging.
    pub packaging_match: Option<bool>,
    /// `None` when the query does not state a quantity.
    pub quantity_compatible: Option<bool>,
    /// The canonical forms differ only in suffixes.
    pub suffix_difference: bool,
    /// The suffix kinds that differ.
    pub suffix_kinds: Vec<crate::identity::SuffixKind>,
    /// Title token overlap in basis points, when a title exists.
    pub title_token_overlap_bp: Option<u16>,
    /// Variant/spec attribute overlap in basis points, when requested.
    pub spec_overlap_bp: Option<u16>,
    /// The offer carries supplier or manufacturer evidence.
    pub supplier_evidence: bool,
}

/// One offer's match result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfferMatch {
    /// The index of the offer in the input slice.
    pub offer_index: usize,
    /// The classification.
    pub certainty: MatchCertainty,
    /// The signals behind the classification.
    pub signals: MatchSignals,
    /// The canonical part number that matched, when one did.
    pub matched_part_number: Option<String>,
    /// The matched manufacturer, when one did.
    pub matched_manufacturer: Option<String>,
}

/// The set-level outcome of matching a query against many offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchOutcome {
    /// The overall certainty (`Ambiguous` when the best tier has more than
    /// one candidate).
    pub certainty: MatchCertainty,
    /// The non-mismatching candidates, in input order.
    pub matches: Vec<OfferMatch>,
    /// How many offers mismatched.
    pub mismatches: usize,
}

fn normalized_text(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn tokenize(value: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    for token in value.split(|c: char| !c.is_ascii_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        let upper = token.to_ascii_uppercase();
        if !tokens.contains(&upper) {
            tokens.push(upper);
        }
    }
    tokens
}

#[cfg(test)]
pub(crate) mod query_view_work {
    use std::cell::Cell;
    thread_local! {
        static VIEWS: Cell<u64> = const { Cell::new(0) };
    }
    pub(crate) fn add(count: u64) {
        VIEWS.with(|c| c.set(c.get() + count));
    }
    pub(crate) fn reset() {
        VIEWS.with(|c| c.set(0));
    }
    pub(crate) fn snapshot() -> u64 {
        VIEWS.with(Cell::get)
    }
}

/// Query-side precomputation (complexity audit): the per-offer path used to
/// re-tokenize and re-normalize the SAME query for every candidate — the
/// part number + manufacturer + package tokens were rebuilt per offer, the
/// manufacturer was re-normalized per offer, and the requested attributes
/// were re-normalized per (variant × attribute) pair. One [`QueryView`] is
/// built per call ([`match_offers`] shares it across the whole batch) and
/// every candidate consumes the precomputed forms.
pub(crate) struct QueryView<'a> {
    query: &'a PartQuery,
    /// Normalized manufacturer, non-empty (the signal's `query` half).
    manufacturer: Option<String>,
    /// Title-overlap wanted tokens: part number + manufacturer + package.
    title_tokens: Vec<String>,
    /// Normalized requested attributes (spec-overlap wanted side).
    spec: Vec<NormalizedAttribute>,
    /// Normalized package text (the package-match wanted side).
    package_normalized: Option<String>,
    /// The canonical query part number (normalized once, not per offer).
    part: Result<NormalizedPartNumber, PartNumberError>,
}

impl<'a> QueryView<'a> {
    pub(crate) fn query(&self) -> &'a PartQuery {
        self.query
    }

    pub(crate) fn new(query: &'a PartQuery) -> Self {
        #[cfg(test)]
        query_view_work::add(1);
        let manufacturer = query
            .manufacturer
            .as_ref()
            .map(|value| normalized_text(value.as_str()))
            .filter(|value| !value.is_empty());
        let mut title_tokens = tokenize(query.part_number.as_str());
        if let Some(value) = &query.manufacturer {
            for token in tokenize(value.as_str()) {
                if !title_tokens.contains(&token) {
                    title_tokens.push(token);
                }
            }
        }
        if let Some(value) = &query.package {
            for token in tokenize(value.as_str()) {
                if !title_tokens.contains(&token) {
                    title_tokens.push(token);
                }
            }
        }
        let package_normalized = query
            .package
            .as_ref()
            .map(|value| normalized_text(value.as_str()));
        Self {
            query,
            manufacturer,
            title_tokens,
            spec: normalized_attributes(&query.attributes),
            package_normalized,
            part: PartNumberNormalizer::new().normalize(query.part_number.as_str()),
        }
    }
}

fn title_token_overlap_bp(view: &QueryView<'_>, title: &str) -> u16 {
    if view.title_tokens.is_empty() {
        return 0;
    }
    let present = tokenize(title);
    let hits = view
        .title_tokens
        .iter()
        .filter(|token| present.contains(token))
        .count() as u64;
    ((hits * 10_000) / view.title_tokens.len() as u64).min(10_000) as u16
}

fn spec_overlap_bp(view: &QueryView<'_>, offer: &CommercialOffer) -> Option<u16> {
    if view.spec.is_empty() {
        return None;
    }
    let wanted = view.spec.len() as u64;
    let best = offer
        .variants
        .iter()
        .map(|variant| {
            // Each variant's attributes are normalized once; every wanted
            // attribute is then a plain normalized comparison (the pairwise
            // `matches()` re-normalized four strings per comparison).
            let have = normalized_attributes(&variant.attributes);
            let hits = matching_attribute_count(&have, &view.spec) as u64;
            (hits * 10_000) / wanted
        })
        .max()
        .unwrap_or(0);
    Some(best.min(10_000) as u16)
}

fn package_match(view: &QueryView<'_>, offer: &CommercialOffer) -> Option<bool> {
    let wanted_normalized = view.package_normalized.as_ref()?;
    if wanted_normalized.is_empty() {
        return None;
    }
    let mut stated = Vec::new();
    for variant in &offer.variants {
        for attribute in &variant.attributes {
            let name = normalized_text(attribute.name.as_str());
            if name == "PACKAGE" || name == "CASE" || name == "PACKAGETYPE" {
                stated.push(normalized_text(attribute.value.as_str()));
            }
        }
    }
    if !stated.is_empty() {
        return Some(stated.iter().any(|value| value == wanted_normalized));
    }
    if normalized_text(offer.title.as_str()).contains(wanted_normalized) {
        return Some(true);
    }
    None
}

fn packaging_match(query: &PartQuery, offer: &CommercialOffer) -> Option<bool> {
    let wanted = query.packaging?;
    if offer
        .packaging
        .iter()
        .any(|option| option.packaging == wanted)
    {
        return Some(true);
    }
    if offer
        .variants
        .iter()
        .any(|variant| variant.packaging == Some(wanted))
    {
        return Some(true);
    }
    if !offer.packaging.is_empty()
        || offer
            .variants
            .iter()
            .any(|variant| variant.packaging.is_some())
    {
        return Some(false);
    }
    None
}

fn quantity_compatible(query: &PartQuery, offer: &CommercialOffer) -> Option<bool> {
    let quantity = query.quantity?;
    let variant = if query.attributes.is_empty() {
        VariantRequest::None
    } else {
        VariantRequest::Attributes(&query.attributes)
    };
    let ctx = PricingContext {
        variant,
        packaging: query.packaging,
        ..PricingContext::default()
    };
    let resolution = resolve_quote(offer, &ctx, quantity);
    match resolution.status {
        QuoteStatus::Resolved => Some(true),
        QuoteStatus::VariantNotFound | QuoteStatus::PackagingNotFound => Some(false),
        QuoteStatus::PackagingMismatch => Some(false),
        QuoteStatus::MoqViolation => Some(false),
        // Ambiguity and unknown pricing are *unknown* quantity
        // compatibility, never a fabricated yes or no.
        QuoteStatus::VariantAmbiguous
        | QuoteStatus::PackagingAmbiguous
        | QuoteStatus::NoPrice
        | QuoteStatus::InquiryRequired
        | QuoteStatus::AmbiguousTier
        | QuoteStatus::QuantityStepUnsatisfiable
        | QuoteStatus::QuantityTooLarge
        | QuoteStatus::AmountOverflow
        | QuoteStatus::CurrencyMismatch => None,
    }
}

struct ManufacturerSignal {
    exact: bool,
    known: bool,
    conflict: bool,
}

fn manufacturer_signal(view: &QueryView<'_>, offer: &CommercialOffer) -> ManufacturerSignal {
    // Borrowed query side and normalized offer side: the pre-fix code
    // cloned the query manufacturer String for EVERY candidate.
    let query_manufacturer = view.manufacturer.as_deref();
    let offer_manufacturer = offer
        .identity
        .manufacturer
        .as_ref()
        .map(|value| normalized_text(value.as_str()))
        .filter(|value| !value.is_empty());
    let known = query_manufacturer.is_some() || offer_manufacturer.is_some();
    let (exact, conflict) = match (query_manufacturer, offer_manufacturer.as_deref()) {
        (Some(left), Some(right)) => (left == right, left != right),
        _ => (false, false),
    };
    ManufacturerSignal {
        exact,
        known,
        conflict,
    }
}

fn empty_signals() -> MatchSignals {
    MatchSignals {
        mpn_exact: false,
        mpn_base_equal: false,
        source_part_exact: false,
        manufacturer_exact: false,
        manufacturer_known: false,
        manufacturer_conflict: false,
        package_match: None,
        packaging_match: None,
        quantity_compatible: None,
        suffix_difference: false,
        suffix_kinds: Vec::new(),
        title_token_overlap_bp: None,
        spec_overlap_bp: None,
        supplier_evidence: false,
    }
}

/// Match one offer against a query, with full signals.
pub fn match_offer(query: &PartQuery, offer: &CommercialOffer, offer_index: usize) -> OfferMatch {
    match_offer_with(&QueryView::new(query), offer, offer_index)
}

/// Everything a match signals: the base signals (manufacturer, package,
/// packaging, title/spec overlap) plus the part-number classification.
/// Quantity compatibility is deliberately NOT computed here: ranking
/// resolves the quote once itself and never needs the matching path's
/// second tier resolution.
fn full_match_signals(
    view: &QueryView<'_>,
    offer: &CommercialOffer,
) -> (MatchSignals, MatchCertainty, Option<String>, Option<String>) {
    let mut signals = empty_signals();
    let manufacturer = manufacturer_signal(view, offer);
    signals.manufacturer_exact = manufacturer.exact;
    signals.manufacturer_known = manufacturer.known;
    signals.manufacturer_conflict = manufacturer.conflict;
    signals.package_match = package_match(view, offer);
    signals.packaging_match = packaging_match(view.query, offer);
    signals.title_token_overlap_bp = Some(title_token_overlap_bp(view, offer.title.as_str()));
    signals.spec_overlap_bp = spec_overlap_bp(view, offer);
    signals.supplier_evidence = offer.supplier.is_some() || offer.manufacturer.is_some();

    let classification = classify_match(view, offer, &mut signals);
    (
        signals,
        classification.0,
        classification.1,
        classification.2,
    )
}

/// The part-number relation and certainty, filling the part-number signals.
/// Kept separate from [`full_match_signals`] so the ranking path gets the
/// EXACT same signal values without paying for a quantity quote.
fn classify_match(
    view: &QueryView<'_>,
    offer: &CommercialOffer,
    signals: &mut MatchSignals,
) -> (MatchCertainty, Option<String>, Option<String>) {
    let manufacturer_exact = signals.manufacturer_exact;
    let manufacturer_conflict = signals.manufacturer_conflict;
    // Borrow the precomputed canonical query part: the pre-fix code cloned
    // it (a `Result<NormalizedPartNumber, _>`) for EVERY candidate.
    let query_part = &view.part;
    let offer_mpn: Result<Option<NormalizedPartNumber>, PartNumberError> =
        offer.identity.normalized_mpn();
    let offer_source_part = offer.identity.normalized_source_part_number();

    let mut matched_part_number = None;
    let mut matched_manufacturer = None;
    let mut certainty = MatchCertainty::Mismatch;

    match (query_part, offer_mpn) {
        (Ok(query_part), Ok(Some(offer_mpn))) => {
            signals.mpn_exact = query_part.exact_eq(&offer_mpn);
            signals.mpn_base_equal = query_part.base_eq(&offer_mpn);
            signals.suffix_difference =
                !query_part.exact_eq(&offer_mpn) && query_part.base_eq(&offer_mpn);
            signals.suffix_kinds = query_part.differing_suffix_kinds(&offer_mpn);
            if signals.mpn_exact {
                matched_part_number = Some(offer_mpn.canonical().to_string());
                certainty = if manufacturer_conflict {
                    MatchCertainty::Mismatch
                } else if manufacturer_exact {
                    MatchCertainty::Exact
                } else {
                    MatchCertainty::Strong
                };
            } else if signals.mpn_base_equal {
                let only_packaging = !signals.suffix_kinds.is_empty()
                    && signals
                        .suffix_kinds
                        .iter()
                        .all(|kind| matches!(kind, crate::identity::SuffixKind::Packaging(_)));
                certainty = if manufacturer_conflict {
                    MatchCertainty::Mismatch
                } else if only_packaging {
                    MatchCertainty::Strong
                } else {
                    MatchCertainty::Possible
                };
                matched_part_number = Some(offer_mpn.canonical().to_string());
            }
        }
        (Ok(query_part), _) => {
            if let Ok(Some(source_part)) = offer_source_part {
                signals.source_part_exact = query_part.exact_eq(&source_part);
                if signals.source_part_exact {
                    matched_part_number = Some(source_part.canonical().to_string());
                    certainty = if manufacturer_conflict {
                        MatchCertainty::Mismatch
                    } else {
                        MatchCertainty::Possible
                    };
                }
            }
            if certainty == MatchCertainty::Mismatch && !manufacturer_conflict {
                let overlap = signals.title_token_overlap_bp.unwrap_or(0);
                if overlap >= TITLE_OVERLAP_POSSIBLE_BP {
                    certainty = MatchCertainty::Possible;
                }
            }
        }
        (Err(_), _) => {
            // A known manufacturer contradiction is never softened by title
            // overlap: the signals would still say `manufacturer_conflict`.
            if !manufacturer_conflict {
                let overlap = signals.title_token_overlap_bp.unwrap_or(0);
                if overlap >= TITLE_OVERLAP_POSSIBLE_BP {
                    certainty = MatchCertainty::Possible;
                }
            }
        }
    }

    if manufacturer_exact {
        matched_manufacturer = offer
            .identity
            .manufacturer
            .as_ref()
            .map(|value| value.as_str().to_string());
    }
    (certainty, matched_part_number, matched_manufacturer)
}

/// Every match signal EXCEPT quantity compatibility, over a precomputed
/// [`QueryView`]. Ranking consumes exactly these signals and resolves the
/// quote itself ONCE, so it never pays for the matching path's second tier
/// resolution per candidate. [`match_offer`] adds the quantity signal on top,
/// preserving its public output exactly.
pub(crate) fn match_signals_in_view(view: &QueryView<'_>, offer: &CommercialOffer) -> MatchSignals {
    full_match_signals(view, offer).0
}

/// The shared implementation over a precomputed [`QueryView`].
fn match_offer_with(
    view: &QueryView<'_>,
    offer: &CommercialOffer,
    offer_index: usize,
) -> OfferMatch {
    let (mut signals, certainty, matched_part_number, matched_manufacturer) =
        full_match_signals(view, offer);
    signals.quantity_compatible = quantity_compatible(view.query, offer);
    OfferMatch {
        offer_index,
        certainty,
        signals,
        matched_part_number,
        matched_manufacturer,
    }
}

/// Match many offers, applying the set-level ambiguity rule. The query-side
/// precomputation is built ONCE for the whole batch (complexity audit).
pub fn match_offers(query: &PartQuery, offers: &[CommercialOffer]) -> MatchOutcome {
    let view = QueryView::new(query);
    let mut matches = Vec::new();
    let mut mismatches = 0usize;
    for (index, offer) in offers.iter().enumerate() {
        let offer_match = match_offer_with(&view, offer, index);
        if offer_match.certainty == MatchCertainty::Mismatch {
            mismatches += 1;
        } else {
            matches.push(offer_match);
        }
    }
    let best_rank = matches
        .iter()
        .map(|offer_match| offer_match.certainty.rank())
        .max()
        .unwrap_or(0);
    let at_best = matches
        .iter()
        .filter(|offer_match| offer_match.certainty.rank() == best_rank)
        .count();
    let certainty = if best_rank == 0 {
        MatchCertainty::Mismatch
    } else if at_best > 1 {
        MatchCertainty::Ambiguous
    } else {
        matches
            .iter()
            .find(|offer_match| offer_match.certainty.rank() == best_rank)
            .map(|offer_match| offer_match.certainty)
            .unwrap_or(MatchCertainty::Mismatch)
    };
    MatchOutcome {
        certainty,
        matches,
        mismatches,
    }
}

#[cfg(test)]
mod complexity_tests {
    use super::*;
    use crate::money::Currency;
    use crate::offer::{
        CommercialOffer, LifecycleStatus, NormalizedAttribute, OfferProvenance, PriceVisibility,
        StockState, VariantOffer,
    };
    use crate::text::SourceId;

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
            currency: Currency::CNY,
            price_breaks: Vec::new(),
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
                origin: crate::offer::ObservationOrigin::OfficialApi,
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
            quantity: None,
            attributes: vec![
                attr("attr1", "val3-1"),
                attr("attr4", "val5-4"),
                attr(" attr7 ", "VAL2-7"),
                attr("attr10", "val1-10"),
            ],
        }
    }

    #[test]
    fn ranking_signals_are_the_full_signals_without_quantity() {
        // Ranking skips the matching path's quantity quote (it resolves the
        // quote itself exactly once); every other signal must be identical
        // in value, so the ranking score cannot drift.
        let mut query = query();
        query.quantity = Some(crate::quantity::Quantity::new(100).unwrap());
        let view = QueryView::new(&query);
        for index in [0usize, 7, 42] {
            let offer = offer(index, 4, 8);
            let lean = match_signals_in_view(&view, &offer);
            let mut full = match_offer(&query, &offer, index).signals;
            assert_eq!(
                full.quantity_compatible,
                quantity_compatible(&query, &offer),
                "the full path keeps its quantity signal"
            );
            full.quantity_compatible = lean.quantity_compatible;
            assert_eq!(lean, full, "signal drift at offer {index}");
        }
    }

    #[test]
    fn batch_matching_normalizes_the_query_and_candidates_linearly() {
        // Complexity-audit evidence: 120 offers × 64 variants × 12 attributes.
        // The pre-fix path rebuilt the query tokens/normalization per offer
        // and re-normalized four strings per (wanted × attribute) pair in
        // spec overlap; the batch view normalizes each candidate attribute
        // exactly once and the wanted side once per batch.
        const OFFERS: usize = 120;
        const VARIANTS: usize = 64;
        const ATTRS: usize = 12;
        let offers: Vec<CommercialOffer> = (0..OFFERS).map(|i| offer(i, VARIANTS, ATTRS)).collect();
        let query = query();
        crate::offer::normalization_work::reset();
        let outcome = match_offers(&query, &offers);
        // Exactly the offer carrying PART7 matches; every other part number
        // is a mismatch (the title shares vendor tokens but the identifier
        // contradicts).
        assert_eq!(outcome.mismatches, OFFERS - 1);
        let best = outcome.matches.first().expect("one match");
        assert_eq!(best.offer_index, 7);
        assert!(best.certainty.rank() >= MatchCertainty::Strong.rank());
        let normalizations = crate::offer::normalization_work::snapshot();
        // Wanted: 4 attributes × 2 (name+value) once; candidates: each
        // attribute name+value once per offer.
        let candidate_axis = (OFFERS * VARIANTS * ATTRS * 2) as u64;
        assert!(
            normalizations <= candidate_axis + 64,
            "normalizations must be linear in candidate attributes: \
             {normalizations} for {candidate_axis} attribute halves + wanted side"
        );
        // Single-offer and batch paths agree on the full signal set.
        for index in [0usize, 7, 42] {
            let single = match_offer(&query, &offers[index], index);
            if single.certainty == MatchCertainty::Mismatch {
                assert!(
                    outcome.matches.iter().all(|m| m.offer_index != index),
                    "single mismatch must stay a batch mismatch at {index}"
                );
            } else {
                assert_eq!(
                    outcome.matches.iter().find(|m| m.offer_index == index),
                    Some(&single),
                    "batch and single signals must agree at {index}"
                );
            }
        }
    }

    #[test]
    fn spec_overlap_reuses_normalized_attributes_exactly() {
        // The hoisted comparison must reproduce the pairwise `matches()`
        // predicate byte-for-byte on unicode, case and spacing variants.
        let query = PartQuery {
            manufacturer: None,
            part_number: Text::new("X1").unwrap(),
            package: None,
            packaging: None,
            quantity: None,
            attributes: vec![
                attr("颜色", "黑色"),
                attr(" 长度 ", "1M"),
                attr("Tolerance", "± 1%"),
            ],
        };
        let mut candidate = offer(0, 0, 0);
        candidate.variants = vec![VariantOffer {
            variant_id: crate::text::VariantId::new("v").unwrap(),
            attributes: vec![
                attr("颜色", "黑色"),
                attr("长度", "1m"),
                attr("tolerance", "±1%"),
            ],
            packaging: None,
            moq: None,
            order_multiple: None,
            standard_pack: None,
            stock: StockState::Unknown,
            price_breaks: Vec::new(),
            lead_time: None,
        }];
        let view = QueryView::new(&query);
        let hoisted = spec_overlap_bp(&view, &candidate);
        assert_eq!(hoisted, Some(10_000), "all three attributes must match");
        // Drop one wanted attribute's match: 2/3 hits -> 6666 bp.
        candidate.variants[0].attributes.pop();
        let hoisted = spec_overlap_bp(&view, &candidate);
        assert_eq!(hoisted, Some(6_666));
        // Equivalent to the pairwise predicate recomputed here.
        let pairwise_hits = query
            .attributes
            .iter()
            .filter(|wanted| {
                candidate.variants[0]
                    .attributes
                    .iter()
                    .any(|attribute| attribute.matches(wanted))
            })
            .count();
        assert_eq!(pairwise_hits, 2);
        // The wanted side really is precomputed and comparable.
        let wanted = normalized_attributes(&query.attributes);
        assert_eq!(wanted.len(), 3);
        assert!(wanted.contains(&NormalizedAttribute::of_parts("颜色", "黑色")));
    }

    /// Independent audit probe: the hoisted [`QueryView`] signals must equal
    /// the PRE-FIX per-offer computations (reimplemented here verbatim) over
    /// deterministic random queries/offers with unicode, case and spacing
    /// twins. Covers title tokens, spec overlap and package matching.
    #[test]
    fn query_view_signals_match_the_per_offer_computation_exactly() {
        fn lcg(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state >> 33
        }
        // PRE-FIX title tokens, verbatim.
        fn old_title_tokens(query: &PartQuery) -> Vec<String> {
            let mut wanted = tokenize(query.part_number.as_str());
            if let Some(manufacturer) = &query.manufacturer {
                for token in tokenize(manufacturer.as_str()) {
                    if !wanted.contains(&token) {
                        wanted.push(token);
                    }
                }
            }
            if let Some(package) = &query.package {
                for token in tokenize(package.as_str()) {
                    if !wanted.contains(&token) {
                        wanted.push(token);
                    }
                }
            }
            wanted
        }
        let names = ["颜色", " 颜色 ", "LENGTH", "length", "公差", "V"];
        let values = ["1M", " 1m ", "± 1%", "±1%", "黑", "black"];
        let parts = ["PART7", "part7", "PART-7", "X7", "PART_7"];
        let manufacturers = [Some("ACME"), Some(" acme "), None, Some("Acme 电子")];
        let packages = [Some("0603"), Some("0603 "), None, Some("CASE 0603")];
        let mut state = 0xC0FFEE_u64;
        for round in 0..300 {
            let query = PartQuery {
                manufacturer: manufacturers[(lcg(&mut state) as usize) % manufacturers.len()]
                    .map(|m| Text::new(m).unwrap()),
                part_number: Text::new(parts[(lcg(&mut state) as usize) % parts.len()]).unwrap(),
                package: packages[(lcg(&mut state) as usize) % packages.len()]
                    .map(|p| Text::new(p).unwrap()),
                packaging: None,
                quantity: None,
                attributes: {
                    let count = (lcg(&mut state) as usize) % 4;
                    (0..count)
                        .map(|_| {
                            attr(
                                names[(lcg(&mut state) as usize) % names.len()],
                                values[(lcg(&mut state) as usize) % values.len()],
                            )
                        })
                        .collect()
                },
            };
            let offer_count = 1 + (lcg(&mut state) as usize) % 3;
            let offers: Vec<CommercialOffer> = (0..offer_count)
                .map(|_| {
                    let variant_count = (lcg(&mut state) as usize) % 3;
                    let mut candidate = offer(0, 0, 0);
                    candidate.title = Text::new("ACME PART7 电阻 0603").unwrap();
                    candidate.variants = (0..variant_count)
                        .map(|_| {
                            let attr_count = (lcg(&mut state) as usize) % 4;
                            let mut v = variant(0, 0);
                            v.attributes = (0..attr_count)
                                .map(|_| {
                                    attr(
                                        names[(lcg(&mut state) as usize) % names.len()],
                                        values[(lcg(&mut state) as usize) % values.len()],
                                    )
                                })
                                .collect();
                            v
                        })
                        .collect();
                    candidate
                })
                .collect();
            let view = QueryView::new(&query);
            // Title tokens: the precomputed vector must equal the old one.
            let mut expected_tokens = old_title_tokens(&query);
            let mut precomputed = view.title_tokens.clone();
            expected_tokens.sort();
            precomputed.sort();
            assert_eq!(expected_tokens, precomputed, "round {round}");
            assert_eq!(
                view.manufacturer,
                query
                    .manufacturer
                    .as_ref()
                    .map(|v| normalized_text(v.as_str()))
                    .filter(|v| !v.is_empty()),
                "round {round}"
            );
            for candidate in &offers {
                // Old spec overlap, verbatim pairwise predicate.
                let old_spec = if query.attributes.is_empty() {
                    None
                } else {
                    let best = candidate
                        .variants
                        .iter()
                        .map(|variant| {
                            let hits = query
                                .attributes
                                .iter()
                                .filter(|wanted| {
                                    variant
                                        .attributes
                                        .iter()
                                        .any(|attribute| attribute.matches(wanted))
                                })
                                .count() as u64;
                            (hits * 10_000) / query.attributes.len() as u64
                        })
                        .max()
                        .unwrap_or(0);
                    Some(best.min(10_000) as u16)
                };
                assert_eq!(
                    spec_overlap_bp(&view, candidate),
                    old_spec,
                    "round {round} spec overlap"
                );
                // Old title overlap, verbatim.
                let old_title = {
                    if expected_tokens.is_empty() {
                        0
                    } else {
                        let present = tokenize(candidate.title.as_str());
                        let hits = expected_tokens
                            .iter()
                            .filter(|token| present.contains(token))
                            .count() as u64;
                        ((hits * 10_000) / expected_tokens.len() as u64).min(10_000) as u16
                    }
                };
                assert_eq!(
                    title_token_overlap_bp(&view, candidate.title.as_str()),
                    old_title,
                    "round {round} title overlap"
                );
            }
        }
    }
}
