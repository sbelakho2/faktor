//! faktor-supply-execution — RFQ generation, bid normalization, negotiation
//! scaffolding and authorization-gated actions.
//!
//! # Normalization
//!
//! A quote is normalized to a comparable buyer outlay:
//!
//! ```text
//! goods            = unit price at the target volume break * target quantity
//! one-time         = tooling + NRE (full cash outlay, non-negative)
//! buyer logistics  = legs the Incoterm assigns to the buyer (estimated)
//! payment value    = (baseline days - quote days) * financed amount
//!                    * cost of capital / 365        (signed)
//! normalized total = goods + one-time + buyer logistics + payment value
//! ```
//!
//! `amortized_per_unit` additionally divides the one-time cost by the declared
//! amortization quantity (or reports typed unavailable if no basis exists).
//! Volume breaks are selected deterministically; a missing buyer leg makes the
//! total unavailable, never zero. Quote inconsistencies are surfaced, not
//! repaired silently.
//!
//! # Authorization
//!
//! [`authorize`] is the only action gate. Without a current, evidenced grant
//! that matches the action, scope, counterparty/currency and amount, the
//! decision is a typed [`AuthorizationDecision::Denied`]. This crate performs
//! no I/O and sends nothing.

use faktor_supply_core::{
    mul_div_round_half_up, validate_text, Availability, Currency, Day, Distribution, EvidenceRef,
    ExactError, Incoterm, Money, PartId, ProcessKind, QualificationKind, Rate, Revision,
    SupplierId, Term, TransportMode, Unavailable, UnavailableReason, Units, MAX_ID_BYTES,
    MAX_TEXT_BYTES,
};
use faktor_supply_logistics::{
    allocate_legs, validate_term_for_mode, CostLeg, InsuranceObligation, LegEstimate,
    LogisticsError, RiskTransferPoint,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum volume breaks in an RFQ or quote.
pub const MAX_VOLUME_BREAKS: usize = 16;
/// Maximum required processes/qualifications in an RFQ.
pub const MAX_RFQ_REQUIREMENTS: usize = 32;
/// Maximum generated questions.
pub const MAX_QUESTIONS: usize = 64;
/// Maximum quotes in one comparison.
pub const MAX_QUOTES: usize = 1_000;
/// Maximum freight-leg estimates on one quote.
pub const MAX_FREIGHT_ESTIMATES: usize = 64;
/// Maximum grants considered for one action.
pub const MAX_GRANTS: usize = 256;
/// Maximum issues per normalized bid.
pub const MAX_ISSUES: usize = 64;
/// Maximum authorization amount (one trillion major units).
pub const MAX_ACTION_AMOUNT: i64 = 1_000_000_000_000_000_000;

/// A refused execution-domain operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecutionError {
    /// Exact arithmetic/value failure.
    #[error(transparent)]
    Exact(#[from] ExactError),
    /// The request is structurally invalid.
    #[error("{0}")]
    Invalid(String),
    /// A list exceeded its bound.
    #[error("{what} exceeds {max} entries")]
    Bound {
        /// The class.
        what: &'static str,
        /// The maximum.
        max: usize,
    },
    /// A quote/package currency mismatch.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch {
        /// Expected.
        left: Currency,
        /// Actual.
        right: Currency,
    },
    /// A negative one-time or unit cost.
    #[error("{0} must not be negative")]
    Negative(&'static str),
    /// Quantity zero.
    #[error("quantity must be positive")]
    ZeroQuantity,
    /// A human approval is required for this consequential action.
    #[error(transparent)]
    Approval(#[from] faktor_approval::ApprovalError),
}

// ------------------------------------------------------------------- RFQ

/// An RFQ request.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RfqRequest {
    /// Stable RFQ id.
    pub id: String,
    /// Revision of the request.
    pub revision: Revision,
    /// Part.
    pub part: PartId,
    /// Description.
    pub description: String,
    /// Currency.
    pub currency: Currency,
    /// Volume breaks to request.
    pub volume_breaks: Vec<Units>,
    /// Required processes.
    #[serde(default)]
    pub required_processes: Vec<ProcessKind>,
    /// Required qualifications.
    #[serde(default)]
    pub required_qualifications: Vec<QualificationKind>,
    /// Requested Incoterm.
    pub incoterm: Incoterm,
    /// Destination country.
    pub destination_country: String,
    /// Requested payment days.
    pub payment_days: u32,
    /// Required-by day.
    pub required_by: Day,
    /// Should-cost target per unit, when known.
    pub should_cost_target: Option<Term<Money>>,
    /// Evidence for the requirement.
    pub requirement_evidence: EvidenceRef,
}

/// A generated RFQ package.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RfqPackage {
    /// RFQ id.
    pub id: String,
    /// Revision.
    pub revision: Revision,
    /// Part.
    pub part: PartId,
    /// Description.
    pub description: String,
    /// Currency.
    pub currency: Currency,
    /// Volume breaks, ascending and unique.
    pub volume_breaks: Vec<Units>,
    /// Required processes.
    pub required_processes: Vec<ProcessKind>,
    /// Required qualifications.
    pub required_qualifications: Vec<QualificationKind>,
    /// Requested Incoterm.
    pub incoterm: Incoterm,
    /// Destination country.
    pub destination_country: String,
    /// Requested payment days.
    pub payment_days: u32,
    /// Required-by day.
    pub required_by: Day,
    /// Should-cost target per unit, when known.
    pub should_cost_target: Option<Term<Money>>,
    /// Deterministic questionnaire.
    pub questions: Vec<String>,
    /// Requirement evidence.
    pub requirement_evidence: EvidenceRef,
}

impl RfqPackage {
    /// Validate bounds and ordering.
    pub fn validate(&self) -> Result<(), ExecutionError> {
        validate_text(&self.id, MAX_ID_BYTES, "RFQ id")?;
        validate_text(&self.description, MAX_TEXT_BYTES, "RFQ description")?;
        validate_text(&self.destination_country, MAX_ID_BYTES, "destination")?;
        validate_breaks(&self.volume_breaks)?;
        if self.required_processes.len() > MAX_RFQ_REQUIREMENTS
            || self.required_qualifications.len() > MAX_RFQ_REQUIREMENTS
        {
            return Err(ExecutionError::Bound {
                what: "RFQ requirements",
                max: MAX_RFQ_REQUIREMENTS,
            });
        }
        if self.questions.len() > MAX_QUESTIONS {
            return Err(ExecutionError::Bound {
                what: "RFQ questions",
                max: MAX_QUESTIONS,
            });
        }
        self.requirement_evidence.validate()?;
        if let Some(target) = &self.should_cost_target {
            target.validate()?;
            if let Some(value) = target.as_known() {
                if value.currency != self.currency {
                    return Err(ExecutionError::CurrencyMismatch {
                        left: self.currency,
                        right: value.currency,
                    });
                }
                if value.micros() < 0 {
                    return Err(ExecutionError::Negative("should-cost target"));
                }
            }
        }
        Ok(())
    }
}

fn validate_breaks(breaks: &[Units]) -> Result<(), ExecutionError> {
    if breaks.is_empty() || breaks.len() > MAX_VOLUME_BREAKS {
        return Err(ExecutionError::Invalid(format!(
            "volume breaks must contain 1..={MAX_VOLUME_BREAKS} entries"
        )));
    }
    let mut previous = None;
    for breakpoint in breaks {
        if breakpoint.is_zero() {
            return Err(ExecutionError::ZeroQuantity);
        }
        if let Some(previous) = previous {
            if *breakpoint <= previous {
                return Err(ExecutionError::Invalid(
                    "volume breaks must be strictly ascending".to_string(),
                ));
            }
        }
        previous = Some(*breakpoint);
    }
    Ok(())
}

/// Generate a deterministic RFQ package and questionnaire.
pub fn generate_rfq(request: &RfqRequest) -> Result<RfqPackage, ExecutionError> {
    validate_text(&request.id, MAX_ID_BYTES, "RFQ id")?;
    validate_text(&request.description, MAX_TEXT_BYTES, "RFQ description")?;
    validate_text(&request.destination_country, MAX_ID_BYTES, "destination")?;
    validate_breaks(&request.volume_breaks)?;
    request.requirement_evidence.validate()?;
    if request.required_processes.len() > MAX_RFQ_REQUIREMENTS
        || request.required_qualifications.len() > MAX_RFQ_REQUIREMENTS
    {
        return Err(ExecutionError::Bound {
            what: "RFQ requirements",
            max: MAX_RFQ_REQUIREMENTS,
        });
    }
    if let Some(target) = &request.should_cost_target {
        target.validate()?;
        if let Some(value) = target.as_known() {
            if value.currency != request.currency {
                return Err(ExecutionError::CurrencyMismatch {
                    left: request.currency,
                    right: value.currency,
                });
            }
            if value.micros() < 0 {
                return Err(ExecutionError::Negative("should-cost target"));
            }
        }
    }
    let breaks = request
        .volume_breaks
        .iter()
        .map(|units| units.get().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut questions = vec![
        format!(
            "Quote unit price in {} for each volume break: {}.",
            request.currency, breaks
        ),
        "State tooling and NRE cost, the amortization quantity and the payment schedule."
            .to_string(),
        format!(
            "State the Incoterm 2020 (requested {}) with the named place, and the transport mode.",
            request.incoterm.code()
        ),
        format!(
            "State the lead-time distribution (P10/P50/P90/P99 days) and the sample size; required by day {}.",
            request.required_by.0
        ),
        "State MOQ, order multiple, price validity, payment days, warranty days and NCNR status."
            .to_string(),
        "State material yield, scrap/rework rates, lot size and tooling ownership.".to_string(),
        "Provide the qualification evidence for every required qualification.".to_string(),
        "Provide process-capability evidence (Cpk/Ppk, sample size) for the required processes."
            .to_string(),
    ];
    if !request.required_processes.is_empty() {
        questions.push(format!(
            "Describe equipment/line capability for: {}.",
            request
                .required_processes
                .iter()
                .map(|process| format!("{process:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !request.required_qualifications.is_empty() {
        questions.push(format!(
            "Submit evidence for: {}.",
            request
                .required_qualifications
                .iter()
                .map(|qualification| format!("{qualification:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if request.should_cost_target.is_some() {
        questions.push(
            "Explain any gap to the should-cost target with a term-by-term breakdown.".to_string(),
        );
    }
    if request.payment_days != 0 {
        questions.push(format!(
            "Confirm financing cost for {} day payment terms.",
            request.payment_days
        ));
    }
    if questions.len() > MAX_QUESTIONS {
        return Err(ExecutionError::Bound {
            what: "RFQ questions",
            max: MAX_QUESTIONS,
        });
    }
    Ok(RfqPackage {
        id: request.id.clone(),
        revision: request.revision,
        part: request.part,
        description: request.description.clone(),
        currency: request.currency,
        volume_breaks: request.volume_breaks.clone(),
        required_processes: request.required_processes.clone(),
        required_qualifications: request.required_qualifications.clone(),
        incoterm: request.incoterm,
        destination_country: request.destination_country.clone(),
        payment_days: request.payment_days,
        required_by: request.required_by,
        should_cost_target: request.should_cost_target.clone(),
        questions,
        requirement_evidence: request.requirement_evidence.clone(),
    })
}

// ----------------------------------------------------------------- quotes

/// A price at one volume break.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePrice {
    /// Quantity at which the price applies.
    pub quantity: Units,
    /// Unit price.
    pub unit_price: Term<Money>,
}

/// A supplier quote.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    /// Stable quote id.
    pub id: String,
    /// Supplier.
    pub supplier: SupplierId,
    /// Part quoted.
    pub part: PartId,
    /// Quote revision.
    pub revision: Revision,
    /// Currency.
    pub currency: Currency,
    /// Volume prices.
    pub prices: Vec<VolumePrice>,
    /// Tooling cost (one-time).
    pub tooling: Term<Money>,
    /// NRE cost (one-time).
    pub nre: Term<Money>,
    /// Quantity over which tooling/NRE are amortized, when stated.
    pub amortization_units: Option<Units>,
    /// Payment days.
    pub payment_days: u32,
    /// Minimum order quantity.
    pub moq: Units,
    /// Order multiple.
    pub multiple: Units,
    /// Lead-time distribution.
    pub lead_time_days: Term<Distribution>,
    /// Price validity end.
    pub valid_until: Option<Day>,
    /// Warranty days.
    pub warranty_days: u32,
    /// Non-cancelable / non-returnable.
    pub ncnr: bool,
    /// Incoterm quoted.
    pub incoterm: Incoterm,
    /// Transport mode assumed for the Incoterm check.
    pub mode: TransportMode,
    /// Buyer-leg freight estimates (used only for buyer-borne legs).
    #[serde(default)]
    pub freight_estimates: Vec<LegEstimate>,
    /// The quote document.
    pub document: EvidenceRef,
}

impl Quote {
    /// Validate bounds, currencies and terms. Structural quote inconsistencies
    /// are reported by [`detect_issues`], not repaired here.
    pub fn validate(&self) -> Result<(), ExecutionError> {
        validate_text(&self.id, MAX_ID_BYTES, "quote id")?;
        self.document.validate()?;
        if self.prices.is_empty() || self.prices.len() > MAX_VOLUME_BREAKS {
            return Err(ExecutionError::Invalid(format!(
                "quote prices must contain 1..={MAX_VOLUME_BREAKS} breaks"
            )));
        }
        if self.multiple.is_zero() {
            return Err(ExecutionError::Invalid(
                "order multiple must be positive".to_string(),
            ));
        }
        if self.payment_days > 3_650 {
            return Err(ExecutionError::Invalid(
                "payment days exceed 10 years".to_string(),
            ));
        }
        if self.freight_estimates.len() > MAX_FREIGHT_ESTIMATES {
            return Err(ExecutionError::Bound {
                what: "freight estimates",
                max: MAX_FREIGHT_ESTIMATES,
            });
        }
        self.tooling.validate()?;
        self.nre.validate()?;
        self.lead_time_days.validate()?;
        if let Some(distribution) = self.lead_time_days.as_known() {
            distribution.validate()?;
        }
        for price in &self.prices {
            if price.quantity.is_zero() {
                return Err(ExecutionError::ZeroQuantity);
            }
            price.unit_price.validate()?;
            if let Some(value) = price.unit_price.as_known() {
                self.check_money(*value, "unit price")?;
            }
        }
        for term in [&self.tooling, &self.nre] {
            if let Some(value) = term.as_known() {
                self.check_money(*value, "one-time cost")?;
            }
        }
        if let Some(units) = self.amortization_units {
            if units.is_zero() {
                return Err(ExecutionError::ZeroQuantity);
            }
        }
        for estimate in &self.freight_estimates {
            estimate.amount.validate()?;
            if let Some(value) = estimate.amount.as_known() {
                self.check_money(*value, "freight estimate")?;
            }
        }
        Ok(())
    }

    fn check_money(&self, value: Money, what: &'static str) -> Result<(), ExecutionError> {
        if value.currency != self.currency {
            return Err(ExecutionError::CurrencyMismatch {
                left: self.currency,
                right: value.currency,
            });
        }
        if value.micros() < 0 {
            return Err(ExecutionError::Negative(what));
        }
        Ok(())
    }
}

/// A detected quote inconsistency.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteIssue {
    /// Stable issue code.
    pub code: String,
    /// Bounded detail.
    pub detail: String,
    /// Whether the issue blocks a usable normalization.
    pub blocking: bool,
}

/// Normalization context.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationContext {
    /// Target quantity.
    pub target_quantity: Units,
    /// Annual cost of capital.
    pub cost_of_capital: Rate,
    /// Baseline payment days (the payment term all bids are normalized to).
    pub baseline_payment_days: u32,
    /// Assessment day.
    pub as_of: Day,
}

impl NormalizationContext {
    /// Validate.
    pub fn validate(&self) -> Result<(), ExecutionError> {
        if self.target_quantity.is_zero() {
            return Err(ExecutionError::ZeroQuantity);
        }
        if self.baseline_payment_days > 3_650 {
            return Err(ExecutionError::Invalid(
                "baseline payment days exceed 10 years".to_string(),
            ));
        }
        Ok(())
    }
}

/// The status of a normalized bid.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BidStatus {
    /// Comparable and complete enough to rank.
    Usable,
    /// A blocking inconsistency was found.
    Blocked {
        /// Reasons.
        reasons: Vec<String>,
    },
    /// The bid is consistent but a required term is missing.
    Incomplete {
        /// Reasons.
        reasons: Vec<String>,
    },
}

/// A normalized bid.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedBid {
    /// Quote id.
    pub quote_id: String,
    /// Supplier.
    pub supplier: SupplierId,
    /// Quote revision.
    pub quote_revision: Revision,
    /// Status.
    pub status: BidStatus,
    /// Selected volume break.
    pub selected_break: Option<Units>,
    /// Unit price at the selected break.
    pub unit_price: Availability<Money>,
    /// Goods value for the target quantity.
    pub goods_value: Availability<Money>,
    /// Full one-time cost (tooling + NRE).
    pub one_time_cost: Availability<Money>,
    /// Buyer-borne logistics under the quoted Incoterm.
    pub buyer_logistics: Availability<Money>,
    /// Buyer legs with no estimate.
    #[serde(default)]
    pub missing_legs: Vec<CostLeg>,
    /// Signed payment-term value.
    pub payment_adjustment: Availability<Money>,
    /// Normalized total.
    pub normalized_total: Availability<Money>,
    /// Normalized per-unit total.
    pub normalized_per_unit: Availability<Money>,
    /// Unit price plus one-time / amortization quantity.
    pub amortized_per_unit: Availability<Money>,
    /// Normalized per-unit minus should-cost target (positive = above target).
    pub should_cost_delta: Availability<Money>,
    /// Variance to the target in ppm (positive = above target).
    pub should_cost_variance_ppm: Availability<i64>,
    /// Lead-time P90.
    pub lead_time_p90_days: Availability<i64>,
    /// Quoted Incoterm.
    pub incoterm: Incoterm,
    /// Quoted transport mode.
    pub mode: TransportMode,
    /// Quoted payment days.
    pub payment_days: u32,
    /// Quoted warranty days.
    pub warranty_days: u32,
    /// Non-cancelable / non-returnable.
    pub ncnr: bool,
    /// Inconsistencies and missing-evidence notes.
    #[serde(default)]
    pub issues: Vec<QuoteIssue>,
}

/// Detect quote inconsistencies for a target quantity. Deterministic order.
pub fn detect_issues(
    quote: &Quote,
    package: &RfqPackage,
    context: &NormalizationContext,
) -> Result<Vec<QuoteIssue>, ExecutionError> {
    quote.validate()?;
    package.validate()?;
    context.validate()?;
    let mut issues = Vec::new();
    if quote.part != package.part {
        issues.push(issue(
            "wrong_part",
            "quote is for a different part than the RFQ",
            true,
        ));
    }
    if quote.currency != package.currency {
        issues.push(issue(
            "currency_mismatch",
            "quote currency differs from the RFQ currency",
            true,
        ));
    }
    if let Err(error) = validate_term_for_mode(quote.incoterm, quote.mode) {
        issues.push(issue("incoterm_mode_mismatch", &error.to_string(), true));
    }
    if let Some(valid_until) = quote.valid_until {
        if valid_until.before(context.as_of) {
            issues.push(issue("expired_quote", "quote validity has passed", true));
        }
    } else {
        issues.push(issue(
            "validity_unknown",
            "quote states no validity date",
            false,
        ));
    }
    if context.target_quantity.get() < quote.moq.get() {
        issues.push(issue(
            "below_moq",
            "target quantity is below the quote MOQ",
            true,
        ));
    }
    if !context
        .target_quantity
        .get()
        .is_multiple_of(quote.multiple.get())
    {
        issues.push(issue(
            "quantity_not_orderable",
            "target quantity is not a multiple of the order multiple",
            true,
        ));
    }
    let mut sorted = quote.prices.clone();
    sorted.sort_by_key(|price| price.quantity);
    let mut previous: Option<(Units, Option<Money>)> = None;
    for price in &sorted {
        if let Some((previous_quantity, previous_price)) = previous {
            if price.quantity == previous_quantity {
                issues.push(issue(
                    "duplicate_break",
                    "two prices share the same volume break",
                    true,
                ));
            }
            if let (Some(earlier), Some(later)) =
                (previous_price, price.unit_price.as_known().copied())
            {
                if later.micros() > earlier.micros() {
                    issues.push(issue(
                        "non_monotonic_price",
                        "unit price rises at a higher volume break",
                        false,
                    ));
                }
            }
        }
        previous = Some((price.quantity, price.unit_price.as_known().copied()));
    }
    if select_price(quote, context.target_quantity).is_none() {
        issues.push(issue(
            "no_applicable_price_break",
            "no volume break at or below the target quantity",
            true,
        ));
    }
    let one_time = one_time(quote)?;
    if let Availability::Known { value } = &one_time {
        if value.micros() < 0 {
            issues.push(issue("negative_one_time", "tooling/NRE is negative", true));
        } else if !value.is_zero() && quote.amortization_units.is_none() {
            issues.push(issue(
                "amortization_basis_missing",
                "one-time cost has no stated amortization quantity",
                false,
            ));
        }
    }
    if quote.lead_time_days.as_known().is_none() {
        issues.push(issue(
            "lead_time_unknown",
            "lead-time distribution is unavailable",
            false,
        ));
    }
    let allocation = allocate_legs(quote.incoterm, quote.currency, &quote.freight_estimates)
        .map_err(|error| ExecutionError::Invalid(error.to_string()))?;
    if !allocation.missing_legs.is_empty() {
        issues.push(issue(
            "missing_buyer_logistics",
            &format!(
                "buyer legs lack estimates: {}",
                allocation
                    .missing_legs
                    .iter()
                    .map(|leg| leg.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            false,
        ));
    }
    issues.sort_by(|left, right| {
        right
            .blocking
            .cmp(&left.blocking)
            .then(left.code.cmp(&right.code))
            .then(left.detail.cmp(&right.detail))
    });
    issues.dedup_by(|left, right| left.code == right.code && left.detail == right.detail);
    if issues.len() > MAX_ISSUES {
        return Err(ExecutionError::Bound {
            what: "quote issues",
            max: MAX_ISSUES,
        });
    }
    Ok(issues)
}

fn issue(code: &str, detail: &str, blocking: bool) -> QuoteIssue {
    QuoteIssue {
        code: code.to_string(),
        detail: detail.to_string(),
        blocking,
    }
}

fn select_price(quote: &Quote, target: Units) -> Option<&VolumePrice> {
    quote
        .prices
        .iter()
        .filter(|price| price.quantity.get() <= target.get())
        .max_by_key(|price| price.quantity)
}

fn one_time(quote: &Quote) -> Result<Availability<Money>, ExecutionError> {
    let tooling = resolve_not_applicable(&quote.tooling, quote.currency);
    let nre = resolve_not_applicable(&quote.nre, quote.currency);
    combine(&[tooling, nre])
}

fn resolve_not_applicable(term: &Term<Money>, currency: Currency) -> Availability<Money> {
    match &term.value {
        Availability::Known { value } => Availability::Known { value: *value },
        Availability::Unavailable { unavailable } if unavailable.is_not_applicable() => {
            Availability::Known {
                value: Money::zero(currency),
            }
        }
        Availability::Unavailable { unavailable } => Availability::Unavailable {
            unavailable: unavailable.clone(),
        },
    }
}

fn combine(values: &[Availability<Money>]) -> Result<Availability<Money>, ExecutionError> {
    let mut total: Option<Money> = None;
    for value in values {
        match value {
            Availability::Known { value } => {
                total = Some(match total {
                    Some(current) => current.checked_add(*value)?,
                    None => *value,
                });
            }
            Availability::Unavailable { unavailable } => {
                return Ok(Availability::Unavailable {
                    unavailable: unavailable.clone(),
                });
            }
        }
    }
    Ok(match total {
        Some(total) => Availability::Known { value: total },
        None => {
            Availability::unavailable(UnavailableReason::MissingEvidence, "no values".to_string())
        }
    })
}

/// Normalize one quote against an RFQ package and context.
pub fn normalize_quote(
    quote: &Quote,
    package: &RfqPackage,
    context: &NormalizationContext,
) -> Result<NormalizedBid, ExecutionError> {
    let issues = detect_issues(quote, package, context)?;
    let blocking = issues
        .iter()
        .filter(|issue| issue.blocking)
        .map(|issue| issue.detail.clone())
        .collect::<Vec<_>>();

    let selected = select_price(quote, context.target_quantity).cloned();
    let unit_price = match &selected {
        Some(price) => price.unit_price.value.clone(),
        None => {
            Availability::unavailable(UnavailableReason::NoCandidate, "no applicable volume break")
        }
    };
    let goods_value = match &unit_price {
        Availability::Known { value } => match value.mul_units(context.target_quantity.get()) {
            Ok(value) => Availability::Known { value },
            Err(error) => {
                Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
            }
        },
        Availability::Unavailable { unavailable } => Availability::Unavailable {
            unavailable: unavailable.clone(),
        },
    };
    let one_time = one_time(quote)?;
    let allocation = allocate_legs(quote.incoterm, quote.currency, &quote.freight_estimates)
        .map_err(|error| ExecutionError::Invalid(error.to_string()))?;
    let buyer_logistics = allocation.buyer_total.clone();
    let missing_legs = allocation.missing_legs.clone();

    let payment_adjustment = match (&goods_value, &one_time) {
        (
            Availability::Known { value: goods },
            Availability::Known {
                value: one_time_value,
            },
        ) => {
            let financed = goods.checked_add(*one_time_value)?;
            let delta_days =
                i128::from(context.baseline_payment_days) - i128::from(quote.payment_days);
            let annual = financed.scale(context.cost_of_capital)?;
            let micros = mul_div_round_half_up(i128::from(annual.micros()), delta_days, 365)
                .ok_or(ExecutionError::Exact(ExactError::Overflow))?;
            let micros =
                i64::try_from(micros).map_err(|_| ExecutionError::Exact(ExactError::Overflow))?;
            Ok::<Availability<Money>, ExecutionError>(Availability::Known {
                value: Money::from_micros(micros, quote.currency)?,
            })
        }
        (Availability::Unavailable { unavailable }, _)
        | (_, Availability::Unavailable { unavailable }) => {
            Ok::<Availability<Money>, ExecutionError>(Availability::Unavailable {
                unavailable: unavailable.clone(),
            })
        }
    }?;

    let normalized_total = combine(&[
        goods_value.clone(),
        one_time.clone(),
        buyer_logistics.clone(),
        payment_adjustment.clone(),
    ])?;
    let normalized_per_unit = match &normalized_total {
        Availability::Known { value } => match value.div_units(context.target_quantity.get()) {
            Ok(value) => Availability::Known { value },
            Err(error) => {
                Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
            }
        },
        Availability::Unavailable { unavailable } => Availability::Unavailable {
            unavailable: unavailable.clone(),
        },
    };
    let amortized_per_unit = match (&unit_price, &one_time) {
        (
            Availability::Known { value: unit },
            Availability::Known {
                value: one_time_value,
            },
        ) => {
            if one_time_value.is_zero() {
                Availability::Known { value: *unit }
            } else {
                match quote.amortization_units {
                    Some(units) if !units.is_zero() => {
                        match one_time_value.div_units(units.get()) {
                            Ok(per_unit) => match unit.checked_add(per_unit) {
                                Ok(value) => Availability::Known { value },
                                Err(error) => Availability::unavailable(
                                    UnavailableReason::Arithmetic,
                                    error.to_string(),
                                ),
                            },
                            Err(error) => Availability::unavailable(
                                UnavailableReason::Arithmetic,
                                error.to_string(),
                            ),
                        }
                    }
                    _ => Availability::unavailable(
                        UnavailableReason::MissingEvidence,
                        "one-time cost has no amortization quantity",
                    ),
                }
            }
        }
        (Availability::Unavailable { unavailable }, _)
        | (_, Availability::Unavailable { unavailable }) => Availability::Unavailable {
            unavailable: unavailable.clone(),
        },
    };

    let (should_cost_delta, should_cost_variance_ppm) =
        match (&normalized_per_unit, package.should_cost_target.as_ref()) {
            (Availability::Known { value: normalized }, Some(target)) => match target.as_known() {
                Some(target) if target.micros() > 0 => match normalized.checked_sub(*target) {
                    Ok(delta) => {
                        let ppm = mul_div_round_half_up(
                            i128::from(delta.micros()),
                            1_000_000,
                            i128::from(target.micros()),
                        )
                        .ok_or(ExecutionError::Exact(ExactError::Overflow))?;
                        match i64::try_from(ppm) {
                            Ok(ppm) => (
                                Availability::Known { value: delta },
                                Availability::Known { value: ppm },
                            ),
                            Err(error) => (
                                Availability::unavailable(
                                    UnavailableReason::Arithmetic,
                                    error.to_string(),
                                ),
                                Availability::unavailable(
                                    UnavailableReason::Arithmetic,
                                    error.to_string(),
                                ),
                            ),
                        }
                    }
                    Err(error) => (
                        Availability::unavailable(UnavailableReason::Arithmetic, error.to_string()),
                        Availability::unavailable(UnavailableReason::Arithmetic, error.to_string()),
                    ),
                },
                Some(_) => (
                    Availability::unavailable(
                        UnavailableReason::OutOfRange,
                        "should-cost target is zero",
                    ),
                    Availability::unavailable(
                        UnavailableReason::OutOfRange,
                        "should-cost target is zero",
                    ),
                ),
                None => (
                    Availability::unavailable(
                        UnavailableReason::MissingEvidence,
                        "should-cost target unavailable",
                    ),
                    Availability::unavailable(
                        UnavailableReason::MissingEvidence,
                        "should-cost target unavailable",
                    ),
                ),
            },
            _ => (
                Availability::unavailable(
                    UnavailableReason::MissingEvidence,
                    "normalized cost or should-cost target unavailable",
                ),
                Availability::unavailable(
                    UnavailableReason::MissingEvidence,
                    "normalized cost or should-cost target unavailable",
                ),
            ),
        };

    let lead_time_p90_days = match quote.lead_time_days.as_known() {
        Some(distribution) => Availability::Known {
            value: distribution.p90,
        },
        None => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "lead-time distribution unavailable",
        ),
    };
    let status = if !blocking.is_empty() {
        BidStatus::Blocked { reasons: blocking }
    } else if normalized_total.is_known() {
        BidStatus::Usable
    } else {
        BidStatus::Incomplete {
            reasons: issues
                .iter()
                .filter(|issue| !issue.blocking)
                .map(|issue| issue.detail.clone())
                .collect(),
        }
    };
    Ok(NormalizedBid {
        quote_id: quote.id.clone(),
        supplier: quote.supplier,
        quote_revision: quote.revision,
        status,
        selected_break: selected.map(|price| price.quantity),
        unit_price,
        goods_value,
        one_time_cost: one_time,
        buyer_logistics,
        missing_legs,
        payment_adjustment,
        normalized_total,
        normalized_per_unit,
        amortized_per_unit,
        should_cost_delta,
        should_cost_variance_ppm,
        lead_time_p90_days,
        incoterm: quote.incoterm,
        mode: quote.mode,
        payment_days: quote.payment_days,
        warranty_days: quote.warranty_days,
        ncnr: quote.ncnr,
        issues,
    })
}

/// A bid comparison.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BidComparison {
    /// Normalized bids in input order.
    pub bids: Vec<NormalizedBid>,
    /// Usable quote ids, cheapest first.
    pub ranked: Vec<String>,
    /// Usable quote ids whose per-unit cost is unavailable.
    pub incomplete: Vec<String>,
    /// Blocked quote ids.
    pub blocked: Vec<String>,
    /// Best usable per-unit cost, if any.
    pub best: Availability<Money>,
}

/// Normalize and rank quotes. Only bids with a known normalized per-unit cost
/// are ranked; unknown bids are listed, never assigned a price.
pub fn compare_bids(
    quotes: &[Quote],
    package: &RfqPackage,
    context: &NormalizationContext,
) -> Result<BidComparison, ExecutionError> {
    if quotes.len() > MAX_QUOTES {
        return Err(ExecutionError::Bound {
            what: "quotes",
            max: MAX_QUOTES,
        });
    }
    package.validate()?;
    context.validate()?;
    let mut seen = std::collections::HashSet::new();
    for quote in quotes {
        if !seen.insert(quote.id.clone()) {
            return Err(ExecutionError::Invalid(format!(
                "duplicate quote id {}",
                quote.id
            )));
        }
    }
    let mut bids = Vec::with_capacity(quotes.len());
    let mut usable = Vec::new();
    let mut incomplete = Vec::new();
    let mut blocked = Vec::new();
    for quote in quotes {
        let bid = normalize_quote(quote, package, context)?;
        match &bid.status {
            BidStatus::Usable => {
                if let Availability::Known { value } = bid.normalized_per_unit {
                    usable.push((bid.quote_id.clone(), value, bid.normalized_total.clone()));
                } else {
                    incomplete.push(bid.quote_id.clone());
                }
            }
            BidStatus::Incomplete { .. } => incomplete.push(bid.quote_id.clone()),
            BidStatus::Blocked { .. } => blocked.push(bid.quote_id.clone()),
        }
        bids.push(bid);
    }
    usable.sort_by(|left, right| {
        left.1
            .micros()
            .cmp(&right.1.micros())
            .then(left.0.cmp(&right.0))
    });
    let ranked = usable
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect::<Vec<_>>();
    let best = match usable.first() {
        Some((_, per_unit, _)) => Availability::Known { value: *per_unit },
        None => Availability::unavailable(UnavailableReason::NoCandidate, "no usable bid"),
    };
    Ok(BidComparison {
        bids,
        ranked,
        incomplete,
        blocked,
        best,
    })
}

// -------------------------------------------------------- incoterm comparison

/// One Incoterm comparison row.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncotermComparison {
    /// Term.
    pub incoterm: Incoterm,
    /// Whether the term is compatible with the quote's mode.
    pub compatible: bool,
    /// Seller-borne legs.
    pub seller_legs: Vec<CostLeg>,
    /// Buyer-borne legs.
    pub buyer_legs: Vec<CostLeg>,
    /// Buyer-borne logistics estimate.
    pub buyer_logistics: Availability<Money>,
    /// Missing buyer leg estimates.
    #[serde(default)]
    pub missing_legs: Vec<CostLeg>,
    /// Total buyer outlay holding the quoted unit price constant.
    pub total: Availability<Money>,
    /// Per-unit buyer outlay holding the quoted unit price constant.
    pub per_unit: Availability<Money>,
    /// Risk transfer.
    pub risk_transfer: RiskTransferPoint,
    /// Insurance obligation.
    pub insurance: InsuranceObligation,
    /// The explicit limitation of the comparison.
    pub note: String,
}

/// Compare all Incoterms for one quote, holding the quoted unit price constant.
/// This is a negotiation scaffold, not a claim that a supplier would quote the
/// same price under a different term.
pub fn compare_incoterms(
    quote: &Quote,
    context: &NormalizationContext,
) -> Result<Vec<IncotermComparison>, ExecutionError> {
    quote.validate()?;
    context.validate()?;
    let selected = select_price(quote, context.target_quantity)
        .ok_or_else(|| ExecutionError::Invalid("no applicable volume break".to_string()))?;
    let unit_price = match selected.unit_price.as_known() {
        Some(value) => *value,
        None => {
            return Err(ExecutionError::Invalid(
                "selected unit price is unavailable".to_string(),
            ))
        }
    };
    let goods = unit_price.mul_units(context.target_quantity.get())?;
    let one_time = one_time(quote)?;
    let one_time = match one_time {
        Availability::Known { value } => value,
        Availability::Unavailable { unavailable } => {
            return Err(ExecutionError::Invalid(
                unavailable
                    .detail
                    .unwrap_or_else(|| "one-time cost unavailable".to_string()),
            ))
        }
    };
    let financed = goods.checked_add(one_time)?;
    let delta_days = i128::from(context.baseline_payment_days) - i128::from(quote.payment_days);
    let annual = financed.scale(context.cost_of_capital)?;
    let adjustment_micros = mul_div_round_half_up(i128::from(annual.micros()), delta_days, 365)
        .ok_or(ExecutionError::Exact(ExactError::Overflow))?;
    let adjustment = Money::from_micros(
        i64::try_from(adjustment_micros)
            .map_err(|_| ExecutionError::Exact(ExactError::Overflow))?,
        quote.currency,
    )?;

    let mut rows = Vec::new();
    for incoterm in [
        Incoterm::Exw,
        Incoterm::Fca,
        Incoterm::Fas,
        Incoterm::Fob,
        Incoterm::Cfr,
        Incoterm::Cif,
        Incoterm::Cpt,
        Incoterm::Cip,
        Incoterm::Dap,
        Incoterm::Dpu,
        Incoterm::Ddp,
    ] {
        let compatible = validate_term_for_mode(incoterm, quote.mode).is_ok();
        let allocation = allocate_legs(incoterm, quote.currency, &quote.freight_estimates)
            .map_err(|error| ExecutionError::Invalid(error.to_string()))?;
        let total = if compatible {
            combine(&[
                Availability::Known { value: goods },
                Availability::Known { value: one_time },
                allocation.buyer_total.clone(),
                Availability::Known { value: adjustment },
            ])?
        } else {
            Availability::unavailable(
                UnavailableReason::NotApplicable,
                "term is not compatible with the quoted transport mode",
            )
        };
        let per_unit = match &total {
            Availability::Known { value } => match value.div_units(context.target_quantity.get()) {
                Ok(value) => Availability::Known { value },
                Err(error) => {
                    Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
                }
            },
            Availability::Unavailable { unavailable } => Availability::Unavailable {
                unavailable: unavailable.clone(),
            },
        };
        rows.push(IncotermComparison {
            incoterm,
            compatible,
            seller_legs: allocation.seller_legs,
            buyer_legs: allocation.buyer_legs,
            buyer_logistics: allocation.buyer_total,
            missing_legs: allocation.missing_legs,
            total,
            per_unit,
            risk_transfer: allocation.risk_transfer,
            insurance: allocation.insurance,
            note: "unit price held constant; seller-borne legs are assumed included in the quoted price".to_string(),
        });
    }
    Ok(rows)
}

// ------------------------------------------------------------------- BATNA

/// One BATNA alternative.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatnaAlternative {
    /// Quote id.
    pub quote_id: String,
    /// Supplier.
    pub supplier: SupplierId,
    /// Normalized per-unit cost, when known.
    pub per_unit: Availability<Money>,
    /// Status.
    pub status: BidStatus,
}

/// BATNA scaffold derived from normalized alternatives.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batna {
    /// Alternatives in deterministic order.
    pub alternatives: Vec<BatnaAlternative>,
    /// Best alternative per-unit cost (the walk-away unit price).
    pub walk_away_per_unit: Availability<Money>,
    /// Best alternative total cost.
    pub walk_away_total: Availability<Money>,
    /// Explicit limitation.
    pub note: String,
}

/// Build a BATNA scaffold. The walk-away is the cheapest usable normalized
/// alternative; without one it is typed unavailable, never a fabricated price.
pub fn batna(comparison: &BidComparison) -> Batna {
    let mut alternatives = comparison
        .bids
        .iter()
        .map(|bid| BatnaAlternative {
            quote_id: bid.quote_id.clone(),
            supplier: bid.supplier,
            per_unit: bid.normalized_per_unit.clone(),
            status: bid.status.clone(),
        })
        .collect::<Vec<_>>();
    alternatives.sort_by(|left, right| left.quote_id.cmp(&right.quote_id));
    let best = comparison
        .bids
        .iter()
        .filter(|bid| matches!(bid.status, BidStatus::Usable))
        .filter_map(|bid| {
            bid.normalized_per_unit
                .as_known()
                .copied()
                .map(|per_unit| (per_unit, bid))
        })
        .min_by(|left, right| {
            left.0
                .micros()
                .cmp(&right.0.micros())
                .then(left.1.quote_id.cmp(&right.1.quote_id))
        });
    let (walk_away_per_unit, walk_away_total) = match best {
        Some((per_unit, bid)) => (
            Availability::Known { value: per_unit },
            bid.normalized_total.clone(),
        ),
        None => (
            Availability::unavailable(UnavailableReason::NoCandidate, "no usable alternative"),
            Availability::unavailable(UnavailableReason::NoCandidate, "no usable alternative"),
        ),
    };
    Batna {
        alternatives,
        walk_away_per_unit,
        walk_away_total,
        note: "BATNA is the cheapest usable normalized bid; it is not a forecast of a future quote"
            .to_string(),
    }
}

/// Negotiation scaffold.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NegotiationPlan {
    /// Should-cost target per unit.
    pub should_cost_target: Availability<Money>,
    /// BATNA.
    pub batna: Batna,
    /// Opening target: the evidenced should-cost target, when known.
    pub opening_target: Availability<Money>,
    /// Reservation price: the BATNA walk-away, when known.
    pub reservation_price: Availability<Money>,
    /// Best current bid minus the should-cost target.
    pub gap_to_target: Availability<Money>,
}

/// Build a negotiation plan using only evidenced targets and normalized bids.
pub fn negotiation_plan(package: &RfqPackage, comparison: &BidComparison) -> NegotiationPlan {
    let should_cost_target = match package.should_cost_target.as_ref() {
        Some(term) => term.value.clone(),
        None => {
            Availability::unavailable(UnavailableReason::MissingEvidence, "no should-cost target")
        }
    };
    let batna = batna(comparison);
    let opening_target = should_cost_target.clone();
    let reservation_price = batna.walk_away_per_unit.clone();
    let gap_to_target = match (&comparison.best, &should_cost_target) {
        (Availability::Known { value: best }, Availability::Known { value: target }) => {
            match best.checked_sub(*target) {
                Ok(value) => Availability::Known { value },
                Err(error) => {
                    Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
                }
            }
        }
        _ => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "gap needs a usable bid and a should-cost target",
        ),
    };
    NegotiationPlan {
        should_cost_target,
        batna,
        opening_target,
        reservation_price,
        gap_to_target,
    }
}

// ------------------------------------------------------------ authorization

/// An externally visible action.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Send an RFQ.
    SendRfq,
    /// Accept an RFQ or its commercial terms.
    AcceptRfq,
    /// Send a message to a counterparty.
    SendMessage,
    /// Create a purchase order.
    CreatePurchaseOrder,
    /// Amend a purchase order.
    AmendPurchaseOrder,
    /// Cancel a purchase order.
    CancelPurchaseOrder,
    /// Accept or change payment terms.
    SetPaymentTerms,
    /// Accept or change contract terms.
    SetContractTerms,
    /// Accept an ambiguous HS/tariff classification.
    SetTariffClassification,
    /// Change inventory policy or safety stock.
    ChangeInventoryPolicy,
}

impl ActionKind {
    /// True when the action carries a monetary commitment.
    pub const fn requires_amount(self) -> bool {
        matches!(self, Self::CreatePurchaseOrder | Self::AmendPurchaseOrder)
    }

    /// The human-approval class this action always requires.
    pub const fn approval_kind(self) -> faktor_approval::ApprovalKind {
        match self {
            Self::SendRfq | Self::AcceptRfq => faktor_approval::ApprovalKind::SupplyRfqAcceptance,
            Self::SendMessage => faktor_approval::ApprovalKind::SupplySupplierMessage,
            Self::CreatePurchaseOrder | Self::AmendPurchaseOrder | Self::CancelPurchaseOrder => {
                faktor_approval::ApprovalKind::SupplyPurchaseOrder
            }
            Self::SetPaymentTerms => faktor_approval::ApprovalKind::SupplyPaymentTerms,
            Self::SetContractTerms => faktor_approval::ApprovalKind::SupplyContractTerms,
            Self::SetTariffClassification => {
                faktor_approval::ApprovalKind::SupplyTariffClassification
            }
            Self::ChangeInventoryPolicy => faktor_approval::ApprovalKind::SupplyInventoryPolicy,
        }
    }
}

/// An explicit grant. No grant is implied by role, scope or a previous action.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationGrant {
    /// Grant id.
    pub id: String,
    /// Action covered.
    pub action: ActionKind,
    /// Exact scope string.
    pub scope: String,
    /// Counterparty restriction, when the grant is limited to one supplier.
    pub counterparty: Option<SupplierId>,
    /// Maximum commitment in the grant currency.
    pub max_amount: Money,
    /// Valid from day.
    pub valid_from: Day,
    /// Valid until day.
    pub valid_until: Day,
    /// Revision of the grant record.
    pub revision: Revision,
    /// Who granted it.
    pub granted_by: String,
    /// Signature / approval evidence.
    pub evidence: EvidenceRef,
}

impl AuthorizationGrant {
    /// Validate the grant and its evidence.
    pub fn validate(&self) -> Result<(), ExecutionError> {
        validate_text(&self.id, MAX_ID_BYTES, "grant id")?;
        validate_text(&self.scope, MAX_TEXT_BYTES, "grant scope")?;
        validate_text(&self.granted_by, MAX_ID_BYTES, "grant authority")?;
        self.evidence.validate()?;
        if self.valid_until.before(self.valid_from) {
            return Err(ExecutionError::Invalid(
                "grant validity ends before it starts".to_string(),
            ));
        }
        if self.max_amount.micros() < 0 {
            return Err(ExecutionError::Negative("grant amount"));
        }
        if self.max_amount.micros() > MAX_ACTION_AMOUNT {
            return Err(ExecutionError::Invalid(
                "grant amount exceeds the action bound".to_string(),
            ));
        }
        Ok(())
    }
}

/// A requested action.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionRequest {
    /// Action.
    pub action: ActionKind,
    /// Scope string.
    pub scope: String,
    /// Counterparty, when applicable.
    pub counterparty: Option<SupplierId>,
    /// Commitment amount, when applicable.
    pub amount: Option<Money>,
    /// Request day.
    pub as_of: Day,
}

/// Why an action was refused.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialReason {
    /// No grant exists.
    NoGrant,
    /// The grant is not yet valid.
    NotYetValid,
    /// The grant has expired.
    Expired,
    /// The grant covers a different action.
    ActionMismatch,
    /// The grant covers a different scope.
    ScopeMismatch,
    /// The grant is limited to a different counterparty.
    CounterpartyMismatch,
    /// A monetary action was requested without an amount.
    AmountMissing,
    /// The requested amount is negative.
    NegativeAmount,
    /// The amount exceeds the grant.
    AmountExceeds,
    /// The amount currency differs from the grant.
    CurrencyMismatch,
    /// A matching grant is structurally invalid.
    InvalidGrant,
    /// The action matches a grant but has no current human approval.
    ApprovalRequired,
    /// The human approval is bound to an older revision/digest.
    ApprovalStale,
    /// The human approval expired.
    ApprovalExpired,
    /// A human explicitly denied the action.
    ApprovalDenied,
}

/// The authorization decision. Nothing in this crate performs the action.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum AuthorizationDecision {
    /// The action is authorized by the named grant.
    Authorized {
        /// Grant id.
        grant_id: String,
        /// Action.
        action: ActionKind,
        /// Counterparty.
        counterparty: Option<SupplierId>,
        /// Amount.
        amount: Option<Money>,
        /// Grant revision.
        grant_revision: Revision,
        /// The human approval request that authorized this exact action.
        approval_request_id: String,
    },
    /// The action is refused.
    Denied {
        /// Typed reason.
        reason: DenialReason,
        /// Bounded detail.
        detail: String,
    },
}

/// The exact action binding a human approval must cover. The entire grant is
/// part of the digest: mutating the scope, counterparty, amount, day, grant
/// revision, grant ceilings, validity window, grantor or grant evidence
/// invalidates a previous approval. Caller-supplied grant text therefore
/// cannot be widened after the human reviewed the action.
pub fn action_digest(
    request: &ActionRequest,
    grant: &AuthorizationGrant,
) -> Result<String, ExecutionError> {
    faktor_approval::content_digest(
        b"faktor.supply.action-binding/v1",
        &(
            request.action,
            &request.scope,
            &request.counterparty,
            &request.amount,
            request.as_of,
            grant,
        ),
    )
    .map_err(ExecutionError::Approval)
}

/// Authorize a requested action against explicit grants. This entry point has
/// no human approvals, so any matching grant still refuses with
/// [`DenialReason::ApprovalRequired`]. Use [`authorize_with_approvals`] with
/// tokens minted by the durable human approval authority.
pub fn authorize(request: &ActionRequest, grants: &[AuthorizationGrant]) -> AuthorizationDecision {
    authorize_with_approvals(request, grants, &[])
}

/// The only action gate. Both an explicit grant AND a current human approval
/// token bound to the exact action must match; there is no default allow and
/// no state-changing side effect.
pub fn authorize_with_approvals(
    request: &ActionRequest,
    grants: &[AuthorizationGrant],
    approvals: &[faktor_approval::ApprovalToken],
) -> AuthorizationDecision {
    if grants.len() > MAX_GRANTS {
        return AuthorizationDecision::Denied {
            reason: DenialReason::NoGrant,
            detail: format!("grant list exceeds {MAX_GRANTS}"),
        };
    }
    if let Err(error) = validate_text(&request.scope, MAX_TEXT_BYTES, "action scope") {
        return AuthorizationDecision::Denied {
            reason: DenialReason::ScopeMismatch,
            detail: error.to_string(),
        };
    }
    if request.action.requires_amount() && request.amount.is_none() {
        return AuthorizationDecision::Denied {
            reason: DenialReason::AmountMissing,
            detail: "the action requires an amount".to_string(),
        };
    }
    if let Some(amount) = request.amount {
        if amount.micros() < 0 {
            return AuthorizationDecision::Denied {
                reason: DenialReason::NegativeAmount,
                detail: "the action amount must not be negative".to_string(),
            };
        }
    }
    let mut candidates = grants
        .iter()
        .filter(|grant| grant.action == request.action)
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .revision
            .cmp(&left.revision)
            .then(left.id.cmp(&right.id))
    });
    let mut last_reason = DenialReason::NoGrant;
    let mut last_detail = "no grant matches the action".to_string();
    for grant in candidates {
        if let Err(error) = grant.validate() {
            last_reason = DenialReason::InvalidGrant;
            last_detail = format!("grant {} is invalid: {error}", grant.id);
            continue;
        }
        if request.as_of.before(grant.valid_from) {
            last_reason = DenialReason::NotYetValid;
            last_detail = format!("grant {} starts on day {}", grant.id, grant.valid_from.0);
            continue;
        }
        if grant.valid_until.before(request.as_of) {
            last_reason = DenialReason::Expired;
            last_detail = format!("grant {} expired on day {}", grant.id, grant.valid_until.0);
            continue;
        }
        if grant.scope != request.scope {
            last_reason = DenialReason::ScopeMismatch;
            last_detail = format!("grant {} covers scope {}", grant.id, grant.scope);
            continue;
        }
        if let Some(granted) = grant.counterparty {
            if Some(granted) != request.counterparty {
                last_reason = DenialReason::CounterpartyMismatch;
                last_detail = format!("grant {} is limited to supplier {}", grant.id, granted.0);
                continue;
            }
        }
        if let Some(amount) = request.amount {
            if amount.currency != grant.max_amount.currency {
                last_reason = DenialReason::CurrencyMismatch;
                last_detail = format!(
                    "grant {} is in {} but the request is in {}",
                    grant.id, grant.max_amount.currency, amount.currency
                );
                continue;
            }
            if amount.micros() > grant.max_amount.micros() {
                last_reason = DenialReason::AmountExceeds;
                last_detail = format!(
                    "amount {} exceeds grant {} maximum {}",
                    amount, grant.id, grant.max_amount
                );
                continue;
            }
        }
        let binding = match action_digest(request, grant) {
            Ok(binding) => binding,
            Err(error) => {
                last_reason = DenialReason::ApprovalRequired;
                last_detail = error.to_string();
                continue;
            }
        };
        let check = faktor_approval::ApprovalCheck::new(
            grant.action.approval_kind(),
            request.scope.clone(),
            grant.revision.get().to_string(),
            binding,
        );
        let Some(approval) = approvals
            .iter()
            .find(|approval| approval.require(&check).is_ok())
        else {
            last_reason = DenialReason::ApprovalRequired;
            last_detail = format!(
                "approval_required: {} on {} at grant revision {} needs a human-operator approval bound to the exact action",
                grant.action.approval_kind(),
                request.scope,
                grant.revision.get()
            );
            continue;
        };
        return AuthorizationDecision::Authorized {
            grant_id: grant.id.clone(),
            action: request.action,
            counterparty: request.counterparty,
            amount: request.amount,
            grant_revision: grant.revision,
            approval_request_id: approval.request_id().to_string(),
        };
    }
    AuthorizationDecision::Denied {
        reason: last_reason,
        detail: last_detail,
    }
}

/// Not applicable TCO-style helper for a quote term.
pub fn unavailable<T>(detail: impl Into<String>) -> Availability<T> {
    Availability::Unavailable {
        unavailable: Unavailable::new(UnavailableReason::MissingEvidence, detail),
    }
}

/// Convenience wrapper around [`LogisticsError`] for callers that do not
/// import the logistics crate directly.
pub fn logistics_error_to_string(error: &LogisticsError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_supply_core::{EvidenceRef, Provenance, SourceKind};
    use faktor_supply_logistics::LegEstimate;

    const CUR: Currency = Currency::USD;
    const DAY: i64 = 20_000;

    fn provenance(id: &str) -> Provenance {
        Provenance::new(
            SourceKind::SupplierDocument,
            EvidenceRef::new(id, Revision(1), None).unwrap(),
            Day(DAY - 2),
            "fixture",
            vec!["quote revision 1".to_string()],
            "a revised quote",
        )
        .unwrap()
    }

    fn term<T>(value: T, id: &str) -> Term<T> {
        Term::evidence(value, provenance(id))
    }

    fn money(amount: &str) -> Money {
        Money::parse(amount, CUR).unwrap()
    }

    fn package() -> RfqPackage {
        generate_rfq(&RfqRequest {
            id: "rfq-1".to_string(),
            revision: Revision(3),
            part: PartId(1),
            description: "machined bracket".to_string(),
            currency: CUR,
            volume_breaks: vec![
                Units::new(100).unwrap(),
                Units::new(1_000).unwrap(),
                Units::new(10_000).unwrap(),
            ],
            required_processes: vec![ProcessKind::Machining],
            required_qualifications: vec![QualificationKind::Ppap],
            incoterm: Incoterm::Ddp,
            destination_country: "US".to_string(),
            payment_days: 30,
            required_by: Day(DAY + 30),
            should_cost_target: Some(term(money("12.50"), "should-cost")),
            requirement_evidence: EvidenceRef::new("rfq-req", Revision(1), None).unwrap(),
        })
        .unwrap()
    }

    fn context() -> NormalizationContext {
        NormalizationContext {
            target_quantity: Units::new(1_000).unwrap(),
            cost_of_capital: Rate::new(80_000).unwrap(),
            baseline_payment_days: 30,
            as_of: Day(DAY),
        }
    }

    fn estimate(leg: CostLeg, amount: &str) -> LegEstimate {
        LegEstimate {
            leg,
            amount: term(money(amount), "freight"),
        }
    }

    fn quote(id: &str, unit_price: &str) -> Quote {
        Quote {
            id: id.to_string(),
            supplier: SupplierId(1),
            part: PartId(1),
            revision: Revision(1),
            currency: CUR,
            prices: vec![
                VolumePrice {
                    quantity: Units::new(100).unwrap(),
                    unit_price: term(money("12.00"), "price-100"),
                },
                VolumePrice {
                    quantity: Units::new(1_000).unwrap(),
                    unit_price: term(money(unit_price), "price-1000"),
                },
            ],
            tooling: term(money("2000.00"), "tooling"),
            nre: Term::not_applicable("no NRE"),
            amortization_units: Some(Units::new(1_000).unwrap()),
            payment_days: 60,
            moq: Units::new(100).unwrap(),
            multiple: Units::new(1).unwrap(),
            lead_time_days: term(
                Distribution {
                    p10: 10,
                    p50: 20,
                    p90: 30,
                    p99: 40,
                    sample_size: 30,
                },
                "lead",
            ),
            valid_until: Some(Day(DAY + 100)),
            warranty_days: 365,
            ncnr: false,
            incoterm: Incoterm::Ddp,
            mode: TransportMode::Road,
            freight_estimates: vec![
                estimate(CostLeg::ExportPackaging, "10.00"),
                estimate(CostLeg::LoadingAtOrigin, "10.00"),
                estimate(CostLeg::OriginInlandFreight, "10.00"),
                estimate(CostLeg::ExportClearance, "10.00"),
                estimate(CostLeg::MainCarriage, "10.00"),
                estimate(CostLeg::CargoInsurance, "20.00"),
                estimate(CostLeg::ImportClearance, "10.00"),
                estimate(CostLeg::DutyAndTax, "10.00"),
                estimate(CostLeg::DestinationInlandFreight, "10.00"),
                estimate(CostLeg::UnloadingAtDestination, "30.00"),
            ],
            document: EvidenceRef::new("quote-doc", Revision(1), None).unwrap(),
        }
    }

    #[test]
    fn rfq_generation_is_deterministic_and_sorted() {
        let first = package();
        let second = package();
        assert_eq!(first, second);
        assert!(first.questions.len() >= 8);
        assert_eq!(first.volume_breaks[0].get(), 100);
        assert_eq!(
            first
                .should_cost_target
                .unwrap()
                .as_known()
                .unwrap()
                .micros(),
            12_500_000
        );
    }

    #[test]
    fn rfq_rejects_unsorted_breaks() {
        let mut request = RfqRequest {
            id: "rfq-bad".to_string(),
            revision: Revision(1),
            part: PartId(1),
            description: "x".to_string(),
            currency: CUR,
            volume_breaks: vec![Units::new(100).unwrap(), Units::new(50).unwrap()],
            required_processes: vec![],
            required_qualifications: vec![],
            incoterm: Incoterm::Exw,
            destination_country: "US".to_string(),
            payment_days: 0,
            required_by: Day(DAY),
            should_cost_target: None,
            requirement_evidence: EvidenceRef::new("r", Revision(1), None).unwrap(),
        };
        assert!(generate_rfq(&request).is_err());
        request.volume_breaks = vec![Units::new(100).unwrap(), Units::new(100).unwrap()];
        assert!(generate_rfq(&request).is_err());
    }

    #[test]
    fn normalization_worked_numeric_case() {
        let bid = normalize_quote(&quote("q1", "10.00"), &package(), &context()).unwrap();
        assert_eq!(bid.status, BidStatus::Usable);
        assert_eq!(bid.unit_price.as_known().unwrap().micros(), 10_000_000);
        assert_eq!(
            bid.goods_value.as_known().unwrap().to_string(),
            "10000.000000 USD"
        );
        assert_eq!(
            bid.one_time_cost.as_known().unwrap().to_string(),
            "2000.000000 USD"
        );
        assert_eq!(
            bid.buyer_logistics.as_known().unwrap().to_string(),
            "50.000000 USD"
        );
        assert_eq!(
            bid.payment_adjustment.as_known().unwrap().to_string(),
            "-78.904110 USD"
        );
        assert_eq!(
            bid.normalized_total.as_known().unwrap().to_string(),
            "11971.095890 USD"
        );
        assert_eq!(
            bid.normalized_per_unit.as_known().unwrap().to_string(),
            "11.971096 USD"
        );
        assert_eq!(
            bid.amortized_per_unit.as_known().unwrap().to_string(),
            "12.000000 USD"
        );
        assert_eq!(
            bid.should_cost_delta.as_known().unwrap().to_string(),
            "-0.528904 USD"
        );
        assert_eq!(*bid.should_cost_variance_ppm.as_known().unwrap(), -42_312);
        assert_eq!(*bid.lead_time_p90_days.as_known().unwrap(), 30);
        assert_eq!(bid.incoterm, Incoterm::Ddp);
        assert_eq!(bid.mode, TransportMode::Road);
        assert_eq!(bid.payment_days, 60);
        assert_eq!(bid.warranty_days, 365);
        assert!(!bid.ncnr);
    }

    #[test]
    fn missing_buyer_leg_is_incomplete_not_zero() {
        let mut quote = quote("q1", "10.00");
        quote.freight_estimates = vec![estimate(CostLeg::CargoInsurance, "20.00")];
        let bid = normalize_quote(&quote, &package(), &context()).unwrap();
        assert!(matches!(bid.status, BidStatus::Incomplete { .. }));
        assert!(bid.missing_legs.contains(&CostLeg::UnloadingAtDestination));
        assert!(!bid.normalized_total.is_known());
    }

    #[test]
    fn expired_quote_is_blocked() {
        let mut quote = quote("q1", "10.00");
        quote.valid_until = Some(Day(DAY - 1));
        let bid = normalize_quote(&quote, &package(), &context()).unwrap();
        assert!(matches!(bid.status, BidStatus::Blocked { .. }));
        assert!(bid.issues.iter().any(|issue| issue.code == "expired_quote"));
    }

    #[test]
    fn non_monotonic_prices_are_surfaced() {
        let mut quote = quote("q1", "12.00");
        quote.prices[1].unit_price = term(money("13.00"), "high");
        let issues = detect_issues(&quote, &package(), &context()).unwrap();
        assert!(issues
            .iter()
            .any(|issue| issue.code == "non_monotonic_price" && !issue.blocking));
    }

    #[test]
    fn duplicate_breaks_and_below_moq_block() {
        let mut duplicated = quote("q1", "10.00");
        duplicated.prices.push(VolumePrice {
            quantity: Units::new(1_000).unwrap(),
            unit_price: term(money("9.00"), "dup"),
        });
        let issues = detect_issues(&duplicated, &package(), &context()).unwrap();
        assert!(issues
            .iter()
            .any(|issue| issue.code == "duplicate_break" && issue.blocking));
        let mut below = context();
        below.target_quantity = Units::new(50).unwrap();
        let bid = normalize_quote(&quote("q2", "10.00"), &package(), &below).unwrap();
        assert!(matches!(bid.status, BidStatus::Blocked { .. }));
        assert!(bid.issues.iter().any(|issue| issue.code == "below_moq"));
    }

    #[test]
    fn missing_amortization_basis_is_surfaced_but_cash_is_known() {
        let mut quote = quote("q1", "10.00");
        quote.amortization_units = None;
        let bid = normalize_quote(&quote, &package(), &context()).unwrap();
        assert!(bid
            .issues
            .iter()
            .any(|issue| { issue.code == "amortization_basis_missing" && !issue.blocking }));
        assert!(bid.normalized_total.is_known());
        assert!(!bid.amortized_per_unit.is_known());
    }

    #[test]
    fn wrong_incoterm_mode_blocks() {
        let mut quote = quote("q1", "10.00");
        quote.incoterm = Incoterm::Fob;
        quote.mode = TransportMode::Air;
        let bid = normalize_quote(&quote, &package(), &context()).unwrap();
        assert!(matches!(bid.status, BidStatus::Blocked { .. }));
        assert!(bid
            .issues
            .iter()
            .any(|issue| issue.code == "incoterm_mode_mismatch"));
    }

    #[test]
    fn compare_bids_ranks_only_known_costs() {
        let mut first = quote("q1", "10.00");
        first.supplier = SupplierId(1);
        let mut second = quote("q2", "9.00");
        second.supplier = SupplierId(2);
        let mut broken = quote("q3", "8.00");
        broken.freight_estimates.clear();
        let comparison = compare_bids(&[first, second, broken], &package(), &context()).unwrap();
        assert_eq!(comparison.ranked, vec!["q2", "q1"]);
        assert!(comparison.incomplete.contains(&"q3".to_string()));
        assert_eq!(comparison.best.as_known().unwrap().micros(), 10_977_671);
    }

    #[test]
    fn incoterm_comparison_holds_price_and_exposes_legs() {
        let quote = quote("q1", "10.00");
        let rows = compare_incoterms(&quote, &context()).unwrap();
        assert_eq!(rows.len(), 11);
        let ddp = rows
            .iter()
            .find(|row| row.incoterm == Incoterm::Ddp)
            .unwrap();
        assert!(ddp.compatible);
        assert_eq!(
            ddp.buyer_legs,
            vec![CostLeg::CargoInsurance, CostLeg::UnloadingAtDestination]
        );
        assert_eq!(ddp.risk_transfer, RiskTransferPoint::DestinationCleared);
        assert_eq!(ddp.insurance, InsuranceObligation::None);
        let cif = rows
            .iter()
            .find(|row| row.incoterm == Incoterm::Cif)
            .unwrap();
        assert!(!cif.compatible); // road mode
        assert!(!cif.total.is_known());
        let fob = rows
            .iter()
            .find(|row| row.incoterm == Incoterm::Fob)
            .unwrap();
        assert!(!fob.compatible);
        let fca = rows
            .iter()
            .find(|row| row.incoterm == Incoterm::Fca)
            .unwrap();
        assert!(fca.compatible);
        assert!(fca.total.is_known());
    }

    #[test]
    fn batna_uses_cheapest_usable_alternative_or_unknown() {
        let mut first = quote("q1", "10.00");
        first.supplier = SupplierId(1);
        let mut second = quote("q2", "9.00");
        second.supplier = SupplierId(2);
        let comparison = compare_bids(&[first, second], &package(), &context()).unwrap();
        let scaffold = batna(&comparison);
        assert_eq!(
            scaffold.walk_away_per_unit.as_known().unwrap().micros(),
            10_977_671
        );
        let empty = BidComparison {
            bids: vec![],
            ranked: vec![],
            incomplete: vec![],
            blocked: vec![],
            best: unavailable("no bid"),
        };
        let empty_scaffold = batna(&empty);
        assert!(!empty_scaffold.walk_away_per_unit.is_known());
    }

    #[test]
    fn negotiation_plan_uses_evidenced_target_and_walk_away() {
        let mut quote = quote("q1", "10.00");
        quote.supplier = SupplierId(1);
        let comparison = compare_bids(&[quote], &package(), &context()).unwrap();
        let plan = negotiation_plan(&package(), &comparison);
        assert_eq!(plan.opening_target.as_known().unwrap().micros(), 12_500_000);
        assert_eq!(
            plan.reservation_price.as_known().unwrap().micros(),
            11_971_096
        );
        assert_eq!(
            plan.gap_to_target.as_known().unwrap().to_string(),
            "-0.528904 USD"
        );
    }

    fn grant(action: ActionKind, scope: &str, max: &str, until: Day) -> AuthorizationGrant {
        AuthorizationGrant {
            id: "grant-1".to_string(),
            action,
            scope: scope.to_string(),
            counterparty: Some(SupplierId(1)),
            max_amount: money(max),
            valid_from: Day(DAY - 1),
            valid_until: until,
            revision: Revision(2),
            granted_by: "procurement-director".to_string(),
            evidence: EvidenceRef::new("grant-sig", Revision(1), None).unwrap(),
        }
    }

    fn request(action: ActionKind, amount: Option<&str>) -> ActionRequest {
        ActionRequest {
            action,
            scope: "rfq:part-1".to_string(),
            counterparty: Some(SupplierId(1)),
            amount: amount.map(money),
            as_of: Day(DAY),
        }
    }

    fn approval(
        request: &ActionRequest,
        grant: &AuthorizationGrant,
    ) -> faktor_approval::ApprovalToken {
        let digest = action_digest(request, grant).unwrap();
        faktor_approval::ApprovalToken::test_token(
            grant.action.approval_kind(),
            request.scope.clone(),
            grant.revision.get().to_string(),
            digest,
        )
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AuthorizeFixture {
        request: ActionRequest,
        grants: Vec<AuthorizationGrant>,
    }

    #[test]
    #[ignore = "[perf] bid comparison at 1k quotes"]
    fn perf_thousand_quotes_compare_in_n_log_n() {
        let quotes: Vec<Quote> = (0..1_000_u32)
            .map(|index| quote(&format!("q{index}"), &format!("{}.00", 10 + index % 90)))
            .collect();
        let started = std::time::Instant::now();
        let comparison = compare_bids(&quotes, &package(), &context()).unwrap();
        let elapsed = started.elapsed();
        eprintln!(
            "perf bids: 1k quotes normalized/ranked in {:?} ({} ranked)",
            elapsed,
            comparison.ranked.len()
        );
        assert!(elapsed.as_secs() < 5, "bid comparison took {elapsed:?}");
    }

    #[test]
    fn json_fixture_authorizes_send_rfq() {
        let fixture: AuthorizeFixture =
            serde_json::from_str(include_str!("../tests/fixtures/authorization.json")).unwrap();
        let approvals: Vec<_> = fixture
            .grants
            .iter()
            .map(|grant| approval(&fixture.request, grant))
            .collect();
        let decision = authorize_with_approvals(&fixture.request, &fixture.grants, &approvals);
        assert!(matches!(decision, AuthorizationDecision::Authorized { .. }));
    }

    #[test]
    fn action_without_grant_is_denied() {
        let decision = authorize(
            &request(ActionKind::CreatePurchaseOrder, Some("100.00")),
            &[],
        );
        assert!(matches!(
            decision,
            AuthorizationDecision::Denied {
                reason: DenialReason::NoGrant,
                ..
            }
        ));
    }

    #[test]
    fn matching_grant_authorizes_exactly() {
        let request = request(ActionKind::CreatePurchaseOrder, Some("100.00"));
        let grants = [grant(
            ActionKind::CreatePurchaseOrder,
            "rfq:part-1",
            "100.00",
            Day(DAY + 10),
        )];
        let approvals = [approval(&request, &grants[0])];
        let decision = authorize_with_approvals(&request, &grants, &approvals);
        match decision {
            AuthorizationDecision::Authorized {
                grant_id, amount, ..
            } => {
                assert_eq!(grant_id, "grant-1");
                assert_eq!(amount.unwrap().micros(), 100_000_000);
            }
            AuthorizationDecision::Denied { .. } => panic!("expected authorization"),
        }
    }

    #[test]
    fn an_approval_cannot_be_replayed_after_any_grant_term_changes() {
        let request = request(ActionKind::CreatePurchaseOrder, Some("100.00"));
        let original = grant(
            ActionKind::CreatePurchaseOrder,
            "rfq:part-1",
            "100.00",
            Day(DAY + 10),
        );
        let token = approval(&request, &original);
        assert!(matches!(
            authorize_with_approvals(
                &request,
                std::slice::from_ref(&original),
                std::slice::from_ref(&token)
            ),
            AuthorizationDecision::Authorized { .. }
        ));

        let mut widened = original.clone();
        widened.max_amount = money("9999.00");
        assert!(matches!(
            authorize_with_approvals(&request, &[widened], std::slice::from_ref(&token)),
            AuthorizationDecision::Denied {
                reason: DenialReason::ApprovalRequired,
                ..
            }
        ));

        let mut revalidated = original.clone();
        revalidated.valid_from = Day(DAY - 2);
        assert!(matches!(
            authorize_with_approvals(&request, &[revalidated], std::slice::from_ref(&token)),
            AuthorizationDecision::Denied {
                reason: DenialReason::ApprovalRequired,
                ..
            }
        ));

        let mut regranted = original;
        regranted.granted_by = "agent-impersonating-procurement".to_string();
        assert!(matches!(
            authorize_with_approvals(&request, &[regranted], &[token]),
            AuthorizationDecision::Denied {
                reason: DenialReason::ApprovalRequired,
                ..
            }
        ));
    }

    #[test]
    fn expired_scope_and_amount_denials_are_typed() {
        let expired = authorize(
            &request(ActionKind::CreatePurchaseOrder, Some("100.00")),
            &[grant(
                ActionKind::CreatePurchaseOrder,
                "rfq:part-1",
                "100.00",
                Day(DAY - 1),
            )],
        );
        assert!(matches!(
            expired,
            AuthorizationDecision::Denied {
                reason: DenialReason::Expired,
                ..
            }
        ));
        let over = authorize(
            &request(ActionKind::CreatePurchaseOrder, Some("101.00")),
            &[grant(
                ActionKind::CreatePurchaseOrder,
                "rfq:part-1",
                "100.00",
                Day(DAY + 1),
            )],
        );
        assert!(matches!(
            over,
            AuthorizationDecision::Denied {
                reason: DenialReason::AmountExceeds,
                ..
            }
        ));
        let wrong_scope = authorize(
            &ActionRequest {
                scope: "rfq:other".to_string(),
                ..request(ActionKind::CreatePurchaseOrder, Some("1.00"))
            },
            &[grant(
                ActionKind::CreatePurchaseOrder,
                "rfq:part-1",
                "100.00",
                Day(DAY + 1),
            )],
        );
        assert!(matches!(
            wrong_scope,
            AuthorizationDecision::Denied {
                reason: DenialReason::ScopeMismatch,
                ..
            }
        ));
        let no_amount = authorize(
            &request(ActionKind::CreatePurchaseOrder, None),
            &[grant(
                ActionKind::CreatePurchaseOrder,
                "rfq:part-1",
                "100.00",
                Day(DAY + 1),
            )],
        );
        assert!(matches!(
            no_amount,
            AuthorizationDecision::Denied {
                reason: DenialReason::AmountMissing,
                ..
            }
        ));
    }

    #[test]
    fn a_negative_amount_is_never_authorized() {
        let decision = authorize(
            &request(ActionKind::CreatePurchaseOrder, Some("-1.00")),
            &[grant(
                ActionKind::CreatePurchaseOrder,
                "rfq:part-1",
                "100.00",
                Day(DAY + 1),
            )],
        );
        assert!(matches!(
            decision,
            AuthorizationDecision::Denied {
                reason: DenialReason::NegativeAmount,
                ..
            }
        ));
    }

    #[test]
    fn grant_without_evidence_is_invalid() {
        let mut bad = grant(
            ActionKind::CreatePurchaseOrder,
            "rfq:part-1",
            "100.00",
            Day(DAY + 1),
        );
        bad.evidence = EvidenceRef::new("", Revision(1), None).unwrap_or_else(|_| EvidenceRef {
            id: "".to_string(),
            revision: Revision(1),
            digest: None,
        });
        let decision = authorize(
            &request(ActionKind::CreatePurchaseOrder, Some("1.00")),
            &[bad],
        );
        assert!(matches!(
            decision,
            AuthorizationDecision::Denied {
                reason: DenialReason::InvalidGrant,
                ..
            }
        ));
    }

    #[test]
    fn message_action_does_not_require_an_amount() {
        let request = request(ActionKind::SendMessage, None);
        let grants = [grant(
            ActionKind::SendMessage,
            "rfq:part-1",
            "1.00",
            Day(DAY + 1),
        )];
        let approvals = [approval(&request, &grants[0])];
        let decision = authorize_with_approvals(&request, &grants, &approvals);
        assert!(matches!(decision, AuthorizationDecision::Authorized { .. }));
    }

    #[test]
    fn matching_grant_without_human_approval_is_refused_typed() {
        let decision = authorize(
            &request(ActionKind::CreatePurchaseOrder, Some("100.00")),
            &[grant(
                ActionKind::CreatePurchaseOrder,
                "rfq:part-1",
                "100.00",
                Day(DAY + 10),
            )],
        );
        match decision {
            AuthorizationDecision::Denied { reason, detail } => {
                assert_eq!(reason, DenialReason::ApprovalRequired);
                assert!(detail.starts_with("approval_required:"), "{detail}");
            }
            AuthorizationDecision::Authorized { .. } => {
                panic!("agent-only grant must not authorize")
            }
        }
    }

    #[test]
    fn stale_human_approval_is_refused_after_the_action_changes() {
        let request = request(ActionKind::CreatePurchaseOrder, Some("100.00"));
        let grants = [grant(
            ActionKind::CreatePurchaseOrder,
            "rfq:part-1",
            "100.00",
            Day(DAY + 10),
        )];
        let approvals = [approval(&request, &grants[0])];
        let mutated = ActionRequest {
            amount: Some(money("99.00")),
            ..request.clone()
        };
        let decision = authorize_with_approvals(&mutated, &grants, &approvals);
        assert!(matches!(
            decision,
            AuthorizationDecision::Denied {
                reason: DenialReason::ApprovalRequired,
                ..
            }
        ));
    }

    #[test]
    fn bad_quote_currency_and_negative_one_time_are_refused() {
        let mut euro = quote("q1", "10.00");
        euro.prices[1].unit_price = term(Money::parse("10.00", Currency::EUR).unwrap(), "eur");
        assert!(matches!(
            normalize_quote(&euro, &package(), &context()),
            Err(ExecutionError::CurrencyMismatch { .. })
        ));
        let mut negative = quote("q2", "10.00");
        negative.tooling = term(money("-10.00"), "negative");
        assert!(matches!(
            normalize_quote(&negative, &package(), &context()),
            Err(ExecutionError::Negative(_))
        ));
    }

    #[test]
    fn an_oversized_freight_estimate_list_is_refused() {
        let mut quote = quote("q1", "10.00");
        quote.freight_estimates = (0..MAX_FREIGHT_ESTIMATES + 1)
            .map(|_| estimate(CostLeg::MainCarriage, "1.00"))
            .collect();
        assert!(matches!(
            quote.validate(),
            Err(ExecutionError::Bound { .. })
        ));
    }
}
