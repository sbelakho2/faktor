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
//! | no identifier match, title token overlap ≥ 90% | `Possible` |
//! | a known identifier or manufacturer contradicts | `Mismatch` |
//!
//! At the set level, two or more candidates at the same best certainty are
//! reported as [`MatchCertainty::Ambiguous`] — a caller must never
//! auto-select among them.

use serde::{Deserialize, Serialize};

use crate::identity::{
    NormalizedPartNumber, PartNumberError, PartNumberNormalizer, MAX_PART_NUMBER_BYTES,
};
use crate::offer::{CommercialOffer, VariantAttribute, MAX_ATTRIBUTES_PER_VARIANT};
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

fn title_token_overlap_bp(query: &PartQuery, title: &str) -> u16 {
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
    if wanted.is_empty() {
        return 0;
    }
    let present = tokenize(title);
    let hits = wanted
        .iter()
        .filter(|token| present.contains(token))
        .count() as u64;
    ((hits * 10_000) / wanted.len() as u64).min(10_000) as u16
}

fn spec_overlap_bp(query: &PartQuery, offer: &CommercialOffer) -> Option<u16> {
    if query.attributes.is_empty() {
        return None;
    }
    let best = offer
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
}

fn package_match(query: &PartQuery, offer: &CommercialOffer) -> Option<bool> {
    let wanted = query.package.as_ref()?;
    let wanted_normalized = normalized_text(wanted.as_str());
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
        return Some(stated.iter().any(|value| value == &wanted_normalized));
    }
    if normalized_text(offer.title.as_str()).contains(&wanted_normalized) {
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

fn manufacturer_signal(query: &PartQuery, offer: &CommercialOffer) -> ManufacturerSignal {
    let query_manufacturer = query
        .manufacturer
        .as_ref()
        .map(|value| normalized_text(value.as_str()))
        .filter(|value| !value.is_empty());
    let offer_manufacturer = offer
        .identity
        .manufacturer
        .as_ref()
        .map(|value| normalized_text(value.as_str()))
        .filter(|value| !value.is_empty());
    let known = query_manufacturer.is_some() || offer_manufacturer.is_some();
    let (exact, conflict) = match (query_manufacturer, offer_manufacturer) {
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
    let mut signals = empty_signals();
    let manufacturer = manufacturer_signal(query, offer);
    let (manufacturer_exact, manufacturer_known, manufacturer_conflict) = (
        manufacturer.exact,
        manufacturer.known,
        manufacturer.conflict,
    );
    signals.manufacturer_exact = manufacturer_exact;
    signals.manufacturer_known = manufacturer_known;
    signals.manufacturer_conflict = manufacturer_conflict;
    signals.package_match = package_match(query, offer);
    signals.packaging_match = packaging_match(query, offer);
    signals.quantity_compatible = quantity_compatible(query, offer);
    signals.title_token_overlap_bp = Some(title_token_overlap_bp(query, offer.title.as_str()));
    signals.spec_overlap_bp = spec_overlap_bp(query, offer);
    signals.supplier_evidence = offer.supplier.is_some() || offer.manufacturer.is_some();

    let query_part = PartNumberNormalizer::new().normalize(query.part_number.as_str());
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
            if certainty == MatchCertainty::Mismatch {
                let overlap = signals.title_token_overlap_bp.unwrap_or(0);
                if overlap >= TITLE_OVERLAP_POSSIBLE_BP {
                    certainty = MatchCertainty::Possible;
                }
            }
        }
        (Err(_), _) => {
            let overlap = signals.title_token_overlap_bp.unwrap_or(0);
            if overlap >= TITLE_OVERLAP_POSSIBLE_BP {
                certainty = MatchCertainty::Possible;
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

    OfferMatch {
        offer_index,
        certainty,
        signals,
        matched_part_number,
        matched_manufacturer,
    }
}

/// Match many offers, applying the set-level ambiguity rule.
pub fn match_offers(query: &PartQuery, offers: &[CommercialOffer]) -> MatchOutcome {
    let mut matches = Vec::new();
    let mut mismatches = 0usize;
    for (index, offer) in offers.iter().enumerate() {
        let offer_match = match_offer(query, offer, index);
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
