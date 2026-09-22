//! Deterministic quote resolution.
//!
//! [`price_at_quantity`] turns an offer (optionally one of its variants and
//! one of its packaging options) plus a requested quantity into a
//! [`QuoteResolution`]. The algorithm is fixed and documented; there is no
//! model arithmetic anywhere:
//!
//! 1. `price_visibility = inquiry_required` short-circuits to
//!    [`QuoteStatus::InquiryRequired`] — an RFQ offer never gets a
//!    fabricated price.
//! 2. **Variant selection**:
//!    - an explicit id that does not exist ⇒ [`QuoteStatus::VariantNotFound`];
//!    - attribute matching with zero matches ⇒ `VariantNotFound`;
//!    - attribute matching with more than one match ⇒
//!      [`QuoteStatus::VariantAmbiguous`] with the candidate ids and the
//!      price range — the cheapest SKU is **never** silently selected;
//!    - no variant request while variants carry prices ⇒
//!      `VariantAmbiguous` (the headline range is not a quote).
//! 3. **Packaging selection**: requested packaging with no matching option
//!    ⇒ [`QuoteStatus::PackagingNotFound`], with more than one ⇒
//!    [`QuoteStatus::PackagingAmbiguous`]; an offer whose packaging options
//!    carry prices requires an explicit selection.
//! 4. **Terms**: the variant's price list wins when it has one, then the
//!    selected packaging option, then the offer level. MOQ, order multiple,
//!    standard pack and stock fall back in the same order.
//! 5. **Quantity**: `quantity < moq` ⇒ [`QuoteStatus::MoqViolation`].
//!    Otherwise the billed quantity is rounded **up** to
//!    `lcm(order_multiple, standard_pack)`; the tier is then selected by the
//!    *billed* quantity (what you actually buy), not the requested one.
//! 6. **Tiers**: the applicable tier with the highest minimum wins;
//!    an account-specific tier beats a public tier at the same minimum; two
//!    tiers with the same minimum and the same specificity are
//!    [`QuoteStatus::AmbiguousTier`], never an arbitrary pick.
//! 7. **Promotions**: explicit, attached to the selected tier, integer-only
//!    ([`PromotionKind`]). An expired promotion is recorded in the notes
//!    and not applied. A bundle price has no uniform unit price, so
//!    `unit_price` stays `None` while the exact `quote.merchandise` is
//!    authoritative.
//! 8. **Quote**: `merchandise` plus whatever landed-cost components the
//!    caller knows; an unknown shipping/tax/duty component is `None`, never
//!    zero, and `Quote::total_is_lower_bound()` says so.

use serde::{Deserialize, Serialize};

use crate::money::{Currency, Money, MoneyError};
use crate::offer::{
    CommercialOffer, PackagingOption, PriceBreak, PriceRange, PriceVisibility, Promotion,
    PromotionKind, StockState, VariantAttribute, VariantOffer,
};
use crate::packaging::PackagingType;
use crate::quantity::{NonZeroQuantity, Quantity, MAX_ORDER_QUANTITY};
use crate::text::{AccountScope, Text, VariantId};

/// How the caller selects a variant.
#[derive(Debug, Clone, Copy)]
pub enum VariantRequest<'a> {
    /// No variant selection: the offer-level terms are used when the
    /// variants do not carry prices, otherwise the resolution is
    /// [`QuoteStatus::VariantAmbiguous`].
    None,
    /// An exact variant id.
    Id(&'a str),
    /// Every attribute must match.
    Attributes(&'a [VariantAttribute]),
}

/// Everything the deterministic pricing algorithm may depend on.
///
/// The context is explicit so that a quote is a pure function of
/// `(offer, context, quantity)`: no clock, account or landed-cost component
/// is read from anywhere else.
#[derive(Debug, Clone, Copy)]
pub struct PricingContext<'a> {
    /// The variant selection.
    pub variant: VariantRequest<'a>,
    /// The requested packaging.
    pub packaging: Option<PackagingType>,
    /// The account scope (account pricing is never applied without it).
    pub account: Option<&'a AccountScope>,
    /// The wall-clock time used for promotion expiry.
    pub now_ms: u64,
    /// Whether explicit promotions are applied.
    pub apply_promotions: bool,
    /// Known shipping cost.
    pub shipping: Option<Money>,
    /// Known tax.
    pub tax: Option<Money>,
    /// Known duty.
    pub duty: Option<Money>,
}

impl Default for PricingContext<'_> {
    fn default() -> Self {
        Self {
            variant: VariantRequest::None,
            packaging: None,
            account: None,
            now_ms: 0,
            apply_promotions: true,
            shipping: None,
            tax: None,
            duty: None,
        }
    }
}

/// The status of a quote resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteStatus {
    /// A quote was produced.
    Resolved,
    /// The variant mapping is ambiguous; see `candidates` and `price_range`.
    VariantAmbiguous,
    /// No variant matched the request.
    VariantNotFound,
    /// More than one packaging option matched.
    PackagingAmbiguous,
    /// No packaging option matched.
    PackagingNotFound,
    /// The variant's packaging and the requested packaging disagree.
    PackagingMismatch,
    /// No applicable price exists.
    NoPrice,
    /// The price requires an RFQ.
    InquiryRequired,
    /// The requested quantity is below the MOQ.
    MoqViolation,
    /// Two tiers with the same minimum apply.
    AmbiguousTier,
    /// No quantity can satisfy the order multiple / standard pack.
    QuantityStepUnsatisfiable,
    /// The rounded quantity exceeds the representable maximum.
    QuantityTooLarge,
    /// The exact amount does not fit the money representation.
    AmountOverflow,
    /// A landed-cost component uses a different currency.
    CurrencyMismatch,
}

/// A recorded, deterministic note about how a quote was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteNote {
    /// The billed quantity was rounded up to the order multiple.
    RoundedUpToOrderMultiple,
    /// The billed quantity was rounded up to the standard pack.
    RoundedUpToStandardPack,
    /// The tier was selected by the billed quantity.
    TierSelectedByBilledQuantity,
    /// An explicit promotion was applied.
    PromotionApplied,
    /// An explicit promotion exists but has expired.
    PromotionExpired,
    /// An account-specific tier was applied.
    AccountPriceApplied,
    /// The known stock is below the billed quantity.
    StockBelowBilledQuantity,
}

/// A promotion that was actually applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedPromotion {
    /// The promotion id.
    pub id: Text<64>,
    /// The promotion kind.
    pub kind: PromotionKind,
    /// The exact amount saved relative to the list price.
    pub saved: Money,
}

/// The tier that was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedTier {
    /// The tier's inclusive minimum.
    pub min_quantity: NonZeroQuantity,
    /// The tier's inclusive maximum.
    pub max_quantity: Option<NonZeroQuantity>,
    /// The tier's list unit price.
    pub list_unit_price: Money,
    /// The account scope of the tier, when it is account-specific.
    pub account_scope: Option<AccountScope>,
}

/// The merchandise total of a quote.
///
/// `shipping`, `tax` and `duty` are `None` when unknown — never zero.
/// `total` is the sum of `merchandise` and every *known* component;
/// [`Quote::total_is_lower_bound`] is true whenever a component is unknown,
/// so an incomplete total can never be mistaken for a landed cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawQuote", into = "RawQuote")]
pub struct Quote {
    /// The merchandise total (billed quantity × effective unit price, or
    /// the exact bundle total).
    pub merchandise: Money,
    /// The shipping cost, when known.
    pub shipping: Option<Money>,
    /// The tax, when known.
    pub tax: Option<Money>,
    /// The duty, when known.
    pub duty: Option<Money>,
    /// `merchandise` plus every known component.
    pub total: Money,
}

impl Quote {
    /// Build a quote, computing `total` from the known components.
    pub fn new(
        merchandise: Money,
        shipping: Option<Money>,
        tax: Option<Money>,
        duty: Option<Money>,
    ) -> Result<Self, QuoteError> {
        let mut total = merchandise;
        for component in [shipping, tax, duty].into_iter().flatten() {
            if component.currency != merchandise.currency {
                return Err(QuoteError::CurrencyMismatch {
                    expected: merchandise.currency,
                    actual: component.currency,
                });
            }
            total = total.checked_add(component)?;
        }
        Ok(Self {
            merchandise,
            shipping,
            tax,
            duty,
            total,
        })
    }

    /// True when every landed-cost component is known.
    pub fn is_complete(&self) -> bool {
        self.shipping.is_some() && self.tax.is_some() && self.duty.is_some()
    }

    /// True when `total` excludes at least one unknown component.
    pub fn total_is_lower_bound(&self) -> bool {
        !self.is_complete()
    }
}

/// A rejected quote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuoteError {
    /// A component uses a different currency.
    #[error("quote component uses {actual} but the merchandise uses {expected}")]
    CurrencyMismatch {
        /// The merchandise currency.
        expected: Currency,
        /// The component currency.
        actual: Currency,
    },
    /// The total does not equal the sum of the known components.
    #[error("quote total {actual} does not equal the exact component sum {expected}")]
    TotalMismatch {
        /// The exact sum.
        expected: Money,
        /// The stated total.
        actual: Money,
    },
    /// Exact arithmetic overflowed.
    #[error(transparent)]
    Money(#[from] MoneyError),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuote {
    merchandise: Money,
    shipping: Option<Money>,
    tax: Option<Money>,
    duty: Option<Money>,
    total: Money,
}

impl TryFrom<RawQuote> for Quote {
    type Error = QuoteError;

    fn try_from(raw: RawQuote) -> Result<Self, Self::Error> {
        let computed = Quote::new(raw.merchandise, raw.shipping, raw.tax, raw.duty)?;
        if computed.total != raw.total {
            return Err(QuoteError::TotalMismatch {
                expected: computed.total,
                actual: raw.total,
            });
        }
        Ok(computed)
    }
}

impl From<Quote> for RawQuote {
    fn from(quote: Quote) -> Self {
        Self {
            merchandise: quote.merchandise,
            shipping: quote.shipping,
            tax: quote.tax,
            duty: quote.duty,
            total: quote.total,
        }
    }
}

/// The full, transparent result of a quote resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawQuoteResolution", into = "RawQuoteResolution")]
pub struct QuoteResolution {
    /// The resolution status.
    pub status: QuoteStatus,
    /// The quote, exactly when the status is [`QuoteStatus::Resolved`].
    pub quote: Option<Quote>,
    /// The effective unit price after promotions, when a uniform one
    /// exists.
    pub unit_price: Option<Money>,
    /// The tier's list unit price before promotions.
    pub list_unit_price: Option<Money>,
    /// The requested quantity.
    pub requested_quantity: Quantity,
    /// The quantity that will actually be purchased.
    pub billed_quantity: Option<Quantity>,
    /// The quantity actually charged (free units excluded).
    pub charged_quantity: Option<Quantity>,
    /// The resolved variant.
    pub variant_id: Option<VariantId>,
    /// The resolved packaging.
    pub packaging: Option<PackagingType>,
    /// The applied tier.
    pub tier: Option<AppliedTier>,
    /// The price range across ambiguous candidates (list/effective unit
    /// prices, never a fabricated quote).
    pub price_range: Option<PriceRange>,
    /// The ambiguous variant candidates.
    pub candidates: Vec<VariantId>,
    /// The ambiguous packaging candidates.
    pub packaging_candidates: Vec<PackagingType>,
    /// The promotions that were applied.
    pub applied_promotions: Vec<AppliedPromotion>,
    /// The applicable MOQ, when one exists.
    pub moq: Option<NonZeroQuantity>,
    /// The applicable order multiple, when one exists.
    pub order_multiple: Option<NonZeroQuantity>,
    /// The applicable standard pack, when one exists.
    pub standard_pack: Option<NonZeroQuantity>,
    /// Deterministic notes about the resolution.
    pub notes: Vec<QuoteNote>,
}

impl QuoteResolution {
    fn bare(status: QuoteStatus, requested_quantity: Quantity) -> Self {
        Self {
            status,
            quote: None,
            unit_price: None,
            list_unit_price: None,
            requested_quantity,
            billed_quantity: None,
            charged_quantity: None,
            variant_id: None,
            packaging: None,
            tier: None,
            price_range: None,
            candidates: Vec::new(),
            packaging_candidates: Vec::new(),
            applied_promotions: Vec::new(),
            moq: None,
            order_multiple: None,
            standard_pack: None,
            notes: Vec::new(),
        }
    }

    /// True when a quote was produced.
    pub fn is_resolved(&self) -> bool {
        self.status == QuoteStatus::Resolved
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuoteResolution {
    status: QuoteStatus,
    quote: Option<Quote>,
    unit_price: Option<Money>,
    list_unit_price: Option<Money>,
    requested_quantity: Quantity,
    billed_quantity: Option<Quantity>,
    charged_quantity: Option<Quantity>,
    variant_id: Option<VariantId>,
    packaging: Option<PackagingType>,
    tier: Option<AppliedTier>,
    price_range: Option<PriceRange>,
    candidates: Vec<VariantId>,
    packaging_candidates: Vec<PackagingType>,
    applied_promotions: Vec<AppliedPromotion>,
    moq: Option<NonZeroQuantity>,
    order_multiple: Option<NonZeroQuantity>,
    standard_pack: Option<NonZeroQuantity>,
    notes: Vec<QuoteNote>,
}

impl TryFrom<RawQuoteResolution> for QuoteResolution {
    type Error = QuoteResolutionError;

    fn try_from(raw: RawQuoteResolution) -> Result<Self, Self::Error> {
        let resolution = QuoteResolution {
            status: raw.status,
            quote: raw.quote,
            unit_price: raw.unit_price,
            list_unit_price: raw.list_unit_price,
            requested_quantity: raw.requested_quantity,
            billed_quantity: raw.billed_quantity,
            charged_quantity: raw.charged_quantity,
            variant_id: raw.variant_id,
            packaging: raw.packaging,
            tier: raw.tier,
            price_range: raw.price_range,
            candidates: raw.candidates,
            packaging_candidates: raw.packaging_candidates,
            applied_promotions: raw.applied_promotions,
            moq: raw.moq,
            order_multiple: raw.order_multiple,
            standard_pack: raw.standard_pack,
            notes: raw.notes,
        };
        resolution.validate()?;
        Ok(resolution)
    }
}

impl From<QuoteResolution> for RawQuoteResolution {
    fn from(resolution: QuoteResolution) -> Self {
        Self {
            status: resolution.status,
            quote: resolution.quote,
            unit_price: resolution.unit_price,
            list_unit_price: resolution.list_unit_price,
            requested_quantity: resolution.requested_quantity,
            billed_quantity: resolution.billed_quantity,
            charged_quantity: resolution.charged_quantity,
            variant_id: resolution.variant_id,
            packaging: resolution.packaging,
            tier: resolution.tier,
            price_range: resolution.price_range,
            candidates: resolution.candidates,
            packaging_candidates: resolution.packaging_candidates,
            applied_promotions: resolution.applied_promotions,
            moq: resolution.moq,
            order_multiple: resolution.order_multiple,
            standard_pack: resolution.standard_pack,
            notes: resolution.notes,
        }
    }
}

impl QuoteResolution {
    /// Validate the status/field consistency of a resolution.
    pub fn validate(&self) -> Result<(), QuoteResolutionError> {
        if self.status != QuoteStatus::Resolved && self.quote.is_some() {
            return Err(QuoteResolutionError::QuoteForUnresolvedStatus {
                status: self.status,
            });
        }
        if self.status == QuoteStatus::Resolved {
            if self.quote.is_none() {
                return Err(QuoteResolutionError::MissingField {
                    status: self.status,
                    field: "quote",
                });
            }
            for (field, present) in [
                ("billed_quantity", self.billed_quantity.is_some()),
                ("charged_quantity", self.charged_quantity.is_some()),
                ("list_unit_price", self.list_unit_price.is_some()),
                ("tier", self.tier.is_some()),
            ] {
                if !present {
                    return Err(QuoteResolutionError::MissingField {
                        status: self.status,
                        field,
                    });
                }
            }
            if self.price_range.is_some() {
                return Err(QuoteResolutionError::UnexpectedField {
                    status: self.status,
                    field: "price_range",
                });
            }
        }
        if matches!(
            self.status,
            QuoteStatus::VariantAmbiguous | QuoteStatus::PackagingAmbiguous
        ) && self.candidates.is_empty()
            && self.packaging_candidates.is_empty()
        {
            return Err(QuoteResolutionError::MissingField {
                status: self.status,
                field: "candidates",
            });
        }
        if self.status == QuoteStatus::MoqViolation && self.moq.is_none() {
            return Err(QuoteResolutionError::MissingField {
                status: self.status,
                field: "moq",
            });
        }
        Ok(())
    }
}

/// A malformed quote resolution.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuoteResolutionError {
    /// A quote is present although the status is not `resolved`.
    #[error("resolution with status {status:?} must not carry a quote")]
    QuoteForUnresolvedStatus {
        /// The status.
        status: QuoteStatus,
    },
    /// A required field is missing for the status.
    #[error("resolution with status {status:?} is missing {field}")]
    MissingField {
        /// The status.
        status: QuoteStatus,
        /// The field.
        field: &'static str,
    },
    /// A field that must be absent is present.
    #[error("resolution with status {status:?} must not carry {field}")]
    UnexpectedField {
        /// The status.
        status: QuoteStatus,
        /// The field.
        field: &'static str,
    },
}

/// The terms that price a request, after variant/packaging precedence.
struct EffectiveTerms<'a> {
    price_breaks: &'a [PriceBreak],
    moq: Option<NonZeroQuantity>,
    order_multiple: Option<NonZeroQuantity>,
    standard_pack: Option<NonZeroQuantity>,
    stock: StockState,
}

fn effective_terms<'a>(
    offer: &'a CommercialOffer,
    variant: Option<&'a VariantOffer>,
    packaging: Option<&'a PackagingOption>,
) -> EffectiveTerms<'a> {
    let price_breaks = match variant {
        Some(variant) if !variant.price_breaks.is_empty() => variant.price_breaks.as_slice(),
        _ => match packaging {
            Some(option) if !option.price_breaks.is_empty() => option.price_breaks.as_slice(),
            _ => offer.price_breaks.as_slice(),
        },
    };
    let variant_terms = variant;
    let packaging_terms = packaging;
    let pick = |from_variant: Option<NonZeroQuantity>,
                from_packaging: Option<NonZeroQuantity>,
                from_offer: Option<NonZeroQuantity>|
     -> Option<NonZeroQuantity> { from_variant.or(from_packaging).or(from_offer) };
    EffectiveTerms {
        price_breaks,
        moq: pick(
            variant_terms.and_then(|variant| variant.moq),
            packaging_terms.and_then(|option| option.moq),
            offer.moq,
        ),
        order_multiple: pick(
            variant_terms.and_then(|variant| variant.order_multiple),
            packaging_terms.and_then(|option| option.order_multiple),
            offer.order_multiple,
        ),
        standard_pack: pick(
            variant_terms.and_then(|variant| variant.standard_pack),
            packaging_terms.and_then(|option| option.standard_pack),
            offer.standard_pack,
        ),
        stock: variant_terms
            .map(|variant| variant.stock)
            .or_else(|| packaging_terms.map(|option| option.stock))
            .unwrap_or(offer.stock),
    }
}

/// A failed deterministic pricing attempt.
enum PricingFailure {
    NoPrice,
    MoqViolation { moq: NonZeroQuantity },
    AmbiguousTier,
    QuantityStepUnsatisfiable,
    QuantityTooLarge,
    AmountOverflow,
}

struct TierOutcome {
    tier: AppliedTier,
    list_unit_price: Money,
    effective_unit_price: Option<Money>,
    merchandise: Money,
    billed_quantity: Quantity,
    charged_quantity: Quantity,
    promotions: Vec<AppliedPromotion>,
    notes: Vec<QuoteNote>,
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

fn lcm(a: u64, b: u64) -> Option<u64> {
    let divisor = gcd(a, b);
    let scaled = (a / divisor).checked_mul(b)?;
    if scaled > MAX_ORDER_QUANTITY {
        return None;
    }
    Some(scaled)
}

fn apply_promotion(
    promotion: &Promotion,
    unit_price: Money,
    billed_quantity: Quantity,
) -> Result<(Money, Quantity, Option<Money>, Money), MoneyError> {
    let list_total = unit_price.checked_mul_quantity(billed_quantity)?;
    match &promotion.kind {
        PromotionKind::PercentOff { basis_points } => {
            let discount = unit_price.checked_mul_basis_points(basis_points.get())?;
            let effective = unit_price.checked_sub(discount)?;
            let total = effective.checked_mul_quantity(billed_quantity)?;
            Ok((
                total,
                billed_quantity,
                Some(effective),
                list_total.checked_sub(total)?,
            ))
        }
        PromotionKind::AmountOff { per_unit } => {
            let effective = unit_price.checked_sub(*per_unit)?;
            let total = effective.checked_mul_quantity(billed_quantity)?;
            Ok((
                total,
                billed_quantity,
                Some(effective),
                list_total.checked_sub(total)?,
            ))
        }
        PromotionKind::FreeUnits {
            pay_units,
            free_units,
        } => {
            let bundle = pay_units.get() + free_units.get();
            let bundles = billed_quantity.as_u64() / bundle;
            let free = bundles * free_units.get();
            let charged =
                Quantity::new(billed_quantity.as_u64() - free).map_err(|_| MoneyError::Overflow)?;
            let total = unit_price.checked_mul_quantity(charged)?;
            Ok((
                total,
                charged,
                Some(unit_price),
                list_total.checked_sub(total)?,
            ))
        }
        PromotionKind::BundlePrice { quantity, price } => {
            let bundle = quantity.get();
            let bundles = billed_quantity.as_u64() / bundle;
            let remainder = billed_quantity.as_u64() % bundle;
            let bundled = price
                .checked_mul_quantity(Quantity::new(bundles).map_err(|_| MoneyError::Overflow)?)?;
            let loose = unit_price.checked_mul_quantity(
                Quantity::new(remainder).map_err(|_| MoneyError::Overflow)?,
            )?;
            let total = bundled.checked_add(loose)?;
            Ok((total, billed_quantity, None, list_total.checked_sub(total)?))
        }
    }
}

fn price_with_terms(
    terms: &EffectiveTerms<'_>,
    quantity: Quantity,
    ctx: &PricingContext<'_>,
) -> Result<TierOutcome, PricingFailure> {
    if let Some(moq) = terms.moq {
        if quantity < moq.as_quantity() {
            return Err(PricingFailure::MoqViolation { moq });
        }
    }
    let order_multiple = terms.order_multiple;
    let standard_pack = terms.standard_pack;
    let step = lcm(
        order_multiple.map(|value| value.get()).unwrap_or(1),
        standard_pack.map(|value| value.get()).unwrap_or(1),
    )
    .and_then(|value| NonZeroQuantity::new(value).ok())
    .ok_or(PricingFailure::QuantityStepUnsatisfiable)?;
    let after_multiple = match order_multiple {
        Some(multiple) => quantity
            .checked_next_multiple(multiple)
            .ok_or(PricingFailure::QuantityTooLarge)?,
        None => quantity,
    };
    let billed_quantity = quantity
        .checked_next_multiple(step)
        .ok_or(PricingFailure::QuantityTooLarge)?;

    let mut notes: Vec<QuoteNote> = Vec::new();
    if after_multiple != quantity {
        notes.push(QuoteNote::RoundedUpToOrderMultiple);
    }
    if billed_quantity != after_multiple {
        notes.push(QuoteNote::RoundedUpToStandardPack);
    }
    if billed_quantity != quantity {
        notes.push(QuoteNote::TierSelectedByBilledQuantity);
    }

    let mut best: Option<&PriceBreak> = None;
    for candidate in terms
        .price_breaks
        .iter()
        .filter(|price_break| price_break.applies_at(billed_quantity, ctx.account))
    {
        match best {
            None => best = Some(candidate),
            Some(current) => {
                if candidate.min_quantity > current.min_quantity {
                    best = Some(candidate);
                } else if candidate.min_quantity == current.min_quantity {
                    let candidate_specific = candidate.is_account_specific();
                    let current_specific = current.is_account_specific();
                    if candidate_specific && !current_specific {
                        best = Some(candidate);
                    } else if candidate_specific == current_specific {
                        return Err(PricingFailure::AmbiguousTier);
                    }
                }
            }
        }
    }
    let Some(tier) = best else {
        return Err(PricingFailure::NoPrice);
    };

    let list_unit_price = tier.unit_price;
    let mut effective_unit_price = Some(list_unit_price);
    let mut merchandise = list_unit_price
        .checked_mul_quantity(billed_quantity)
        .map_err(|_| PricingFailure::AmountOverflow)?;
    let mut charged_quantity = billed_quantity;
    let mut promotions: Vec<AppliedPromotion> = Vec::new();
    if ctx.apply_promotions {
        if let Some(promotion) = &tier.promotion {
            if promotion.is_active_at(ctx.now_ms) {
                let (total, charged, unit, saved) =
                    apply_promotion(promotion, list_unit_price, billed_quantity)
                        .map_err(|_| PricingFailure::AmountOverflow)?;
                merchandise = total;
                charged_quantity = charged;
                effective_unit_price = unit;
                promotions.push(AppliedPromotion {
                    id: promotion.id.clone(),
                    kind: promotion.kind.clone(),
                    saved,
                });
                notes.push(QuoteNote::PromotionApplied);
            } else {
                notes.push(QuoteNote::PromotionExpired);
            }
        }
    }
    if tier.is_account_specific() {
        notes.push(QuoteNote::AccountPriceApplied);
    }
    if let Some(available) = terms.stock.available_quantity() {
        if available < billed_quantity.as_u64() {
            notes.push(QuoteNote::StockBelowBilledQuantity);
        }
    }
    Ok(TierOutcome {
        tier: AppliedTier {
            min_quantity: tier.min_quantity,
            max_quantity: tier.max_quantity,
            list_unit_price,
            account_scope: tier.account_scope.clone(),
        },
        list_unit_price,
        effective_unit_price,
        merchandise,
        billed_quantity,
        charged_quantity,
        promotions,
        notes,
    })
}

fn candidate_unit_price(
    offer: &CommercialOffer,
    variant: Option<&VariantOffer>,
    packaging: Option<&PackagingOption>,
    quantity: Quantity,
    ctx: &PricingContext<'_>,
) -> Option<Money> {
    let terms = effective_terms(offer, variant, packaging);
    let outcome = price_with_terms(&terms, quantity, ctx).ok()?;
    Some(
        outcome
            .effective_unit_price
            .unwrap_or(outcome.list_unit_price),
    )
}

fn candidate_range(prices: impl Iterator<Item = Money>) -> Option<PriceRange> {
    let mut range: Option<PriceRange> = None;
    for price in prices {
        range = Some(match range {
            Some(existing) => existing.widen(price).ok()?,
            None => PriceRange::new(price, price).ok()?,
        });
    }
    range
}

fn variant_ambiguous(
    offer: &CommercialOffer,
    requested_quantity: Quantity,
    ctx: &PricingContext<'_>,
) -> QuoteResolution {
    let candidates: Vec<VariantId> = offer
        .variants
        .iter()
        .map(|variant| variant.variant_id.clone())
        .collect();
    let price_range = candidate_range(offer.variants.iter().filter_map(|variant| {
        candidate_unit_price(offer, Some(variant), None, requested_quantity, ctx)
    }));
    let mut resolution = QuoteResolution::bare(QuoteStatus::VariantAmbiguous, requested_quantity);
    resolution.candidates = candidates;
    resolution.price_range = price_range;
    resolution
}

fn resolve_packaging(
    offer: &CommercialOffer,
    requested: Option<PackagingType>,
) -> Result<Option<&PackagingOption>, (QuoteStatus, Vec<PackagingType>)> {
    if offer.packaging.is_empty() {
        return Ok(None);
    }
    let priced_options: Vec<&PackagingOption> = offer
        .packaging
        .iter()
        .filter(|option| !option.price_breaks.is_empty())
        .collect();
    match requested {
        None => {
            if priced_options.is_empty() {
                Ok(None)
            } else {
                Err((
                    QuoteStatus::PackagingAmbiguous,
                    offer
                        .packaging
                        .iter()
                        .map(|option| option.packaging)
                        .collect(),
                ))
            }
        }
        Some(packaging) => {
            let matching = offer.packaging_options(packaging);
            match matching.len() {
                0 => Err((QuoteStatus::PackagingNotFound, vec![packaging])),
                1 => Ok(Some(matching[0])),
                _ => Err((
                    QuoteStatus::PackagingAmbiguous,
                    matching.iter().map(|option| option.packaging).collect(),
                )),
            }
        }
    }
}

/// Resolve a quote with an explicit pricing context.
pub fn resolve_quote(
    offer: &CommercialOffer,
    ctx: &PricingContext<'_>,
    quantity: Quantity,
) -> QuoteResolution {
    if offer.price_visibility == PriceVisibility::InquiryRequired {
        return QuoteResolution::bare(QuoteStatus::InquiryRequired, quantity);
    }

    let variant: Option<&VariantOffer> = match ctx.variant {
        VariantRequest::None => {
            if offer
                .variants
                .iter()
                .any(|variant| !variant.price_breaks.is_empty())
            {
                return variant_ambiguous(offer, quantity, ctx);
            }
            None
        }
        VariantRequest::Id(variant_id) => match offer.variant_by_id(variant_id) {
            Some(variant) => Some(variant),
            None => return QuoteResolution::bare(QuoteStatus::VariantNotFound, quantity),
        },
        VariantRequest::Attributes(attributes) => {
            let matching = offer.variants_matching(attributes);
            match matching.len() {
                0 => return QuoteResolution::bare(QuoteStatus::VariantNotFound, quantity),
                1 => Some(matching[0]),
                _ => {
                    let mut resolution = variant_ambiguous(offer, quantity, ctx);
                    resolution.candidates = matching
                        .iter()
                        .map(|variant| variant.variant_id.clone())
                        .collect();
                    resolution.price_range =
                        candidate_range(matching.iter().filter_map(|variant| {
                            candidate_unit_price(offer, Some(variant), None, quantity, ctx)
                        }));
                    return resolution;
                }
            }
        }
    };

    let packaging = match resolve_packaging(offer, ctx.packaging) {
        Ok(packaging) => packaging,
        Err((status, packaging_candidates)) => {
            let mut resolution = QuoteResolution::bare(status, quantity);
            if let Some(variant) = variant {
                resolution.variant_id = Some(variant.variant_id.clone());
            }
            resolution.packaging_candidates = packaging_candidates;
            resolution.price_range = candidate_range(offer.packaging.iter().filter_map(|option| {
                candidate_unit_price(offer, variant, Some(option), quantity, ctx)
            }));
            return resolution;
        }
    };

    if let (Some(variant), Some(requested)) = (variant, ctx.packaging) {
        if let Some(variant_packaging) = variant.packaging {
            if variant_packaging != requested {
                let mut resolution =
                    QuoteResolution::bare(QuoteStatus::PackagingMismatch, quantity);
                resolution.variant_id = Some(variant.variant_id.clone());
                resolution.packaging = Some(requested);
                return resolution;
            }
        }
    }

    let terms = effective_terms(offer, variant, packaging);
    let mut resolution = QuoteResolution::bare(QuoteStatus::NoPrice, quantity);
    resolution.variant_id = variant.map(|variant| variant.variant_id.clone());
    resolution.packaging = packaging.map(|option| option.packaging);
    resolution.moq = terms.moq;
    resolution.order_multiple = terms.order_multiple;
    resolution.standard_pack = terms.standard_pack;

    let outcome = match price_with_terms(&terms, quantity, ctx) {
        Ok(outcome) => outcome,
        Err(PricingFailure::NoPrice) => return resolution,
        Err(PricingFailure::MoqViolation { moq }) => {
            resolution.status = QuoteStatus::MoqViolation;
            resolution.moq = Some(moq);
            return resolution;
        }
        Err(PricingFailure::AmbiguousTier) => {
            resolution.status = QuoteStatus::AmbiguousTier;
            return resolution;
        }
        Err(PricingFailure::QuantityStepUnsatisfiable) => {
            resolution.status = QuoteStatus::QuantityStepUnsatisfiable;
            return resolution;
        }
        Err(PricingFailure::QuantityTooLarge) => {
            resolution.status = QuoteStatus::QuantityTooLarge;
            return resolution;
        }
        Err(PricingFailure::AmountOverflow) => {
            resolution.status = QuoteStatus::AmountOverflow;
            return resolution;
        }
    };

    let quote = match Quote::new(outcome.merchandise, ctx.shipping, ctx.tax, ctx.duty) {
        Ok(quote) => quote,
        Err(QuoteError::CurrencyMismatch { .. }) => {
            resolution.status = QuoteStatus::CurrencyMismatch;
            return resolution;
        }
        Err(_) => {
            resolution.status = QuoteStatus::AmountOverflow;
            return resolution;
        }
    };
    resolution.status = QuoteStatus::Resolved;
    resolution.quote = Some(quote);
    resolution.unit_price = outcome.effective_unit_price;
    resolution.list_unit_price = Some(outcome.list_unit_price);
    resolution.billed_quantity = Some(outcome.billed_quantity);
    resolution.charged_quantity = Some(outcome.charged_quantity);
    resolution.tier = Some(outcome.tier);
    resolution.applied_promotions = outcome.promotions;
    resolution.notes = outcome.notes;
    resolution
}

/// The deterministic spec entry point: price `quantity` units of `offer`
/// (optionally a specific variant) with no account, no landed costs and
/// promotions evaluated at `now_ms = 0`.
pub fn price_at_quantity(
    offer: &CommercialOffer,
    variant: VariantRequest<'_>,
    quantity: Quantity,
) -> QuoteResolution {
    let ctx = PricingContext {
        variant,
        ..PricingContext::default()
    };
    resolve_quote(offer, &ctx, quantity)
}
