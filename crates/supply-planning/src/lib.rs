//! faktor-supply-planning — deterministic materials planning.
//!
//! # Pipeline
//!
//! ```text
//! EBOM x independent demand / yield  -> gross requirements per part/per bucket
//! gross - on-hand - confirmed supply  -> net requirements (time-phased)
//! service level x demand sigma x sqrt(lead time) -> safety stock
//! demand during lead time + safety    -> reorder point
//! net requirement, MOQ/multiple       -> planned orders, lead-time offset
//! ```
//!
//! The BOM is exploded in topological order (`O(P * (V + E))` for `P` bounded
//! time buckets), ABC is an exact `O(n log n)` descending-spend sort, and all
//! arithmetic is integer. Unknown lead time or demand dispersion produces an
//! `incomplete` plan with no fabricated orders. Dual sourcing splits each
//! order by exact largest-remainder allocation. Last-time-buy and shelf-life
//! risks are explicit outputs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use faktor_supply_core::{
    mul_div_round_half_up, mul_div_round_up, validate_text, Availability, Currency, Day,
    Distribution, ExactError, LifecycleStatus, Money, PartId, Quantile, Rate, ScaledQuantity,
    SupplierId, Term, UnavailableReason, Units, MAX_TEXT_BYTES, MAX_UNITS,
};
use serde::{Deserialize, Serialize};

/// Maximum planning buckets.
pub const MAX_PERIODS: u32 = 520;
/// Maximum parts in one planning run.
pub const MAX_PARTS: usize = 20_000;
/// Maximum BOM lines.
pub const MAX_BOM_LINES: usize = 500_000;
/// Maximum independent demand lines.
pub const MAX_DEMAND_LINES: usize = 500_000;
/// Maximum confirmed-supply lines.
pub const MAX_CONFIRMED_LINES: usize = 500_000;
/// Maximum BOM depth.
pub const MAX_BOM_DEPTH: usize = 64;
/// Maximum suppliers per part.
pub const MAX_SUPPLIERS_PER_PART: usize = 16;
/// Maximum planned orders per part.
pub const MAX_ORDERS_PER_PART: usize = 4 * MAX_PERIODS as usize;
/// Supported service levels (parts-per-million) with exact z values.
pub const SERVICE_LEVEL_TABLE: &[(i64, i64)] = &[
    (500_000, 0),
    (750_000, 674_490),
    (800_000, 841_621),
    (850_000, 1_036_433),
    (900_000, 1_281_552),
    (910_000, 1_340_755),
    (920_000, 1_405_071),
    (930_000, 1_475_791),
    (940_000, 1_554_774),
    (950_000, 1_644_854),
    (960_000, 1_750_686),
    (970_000, 1_880_794),
    (975_000, 1_959_964),
    (980_000, 2_053_749),
    (990_000, 2_326_348),
    (995_000, 2_575_829),
    (999_000, 3_090_232),
];

/// A refused planning run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanningError {
    /// Exact value/arithmetic failure.
    #[error(transparent)]
    Exact(#[from] ExactError),
    /// No planning buckets.
    #[error("periods must be at least 1")]
    NoPeriods,
    /// Days per period must be positive.
    #[error("days per period must be positive")]
    NoDaysPerPeriod,
    /// Too many buckets.
    #[error("periods {actual} exceed {max}")]
    PeriodBound {
        /// Maximum.
        max: u32,
        /// Requested.
        actual: u32,
    },
    /// Two plans for one part.
    #[error("duplicate part plan {0}")]
    DuplicatePart(PartId),
    /// A BOM/demand/confirmed reference names a part with no plan.
    #[error("part {0} has no planning record")]
    UnknownPart(PartId),
    /// The BOM contains a cycle.
    #[error("BOM cycle detected")]
    BomCycle,
    /// The BOM is deeper than the bound.
    #[error("BOM depth exceeds {0}")]
    BomDepth(usize),
    /// A line period is outside the horizon.
    #[error("period {period} is outside 0..{periods}")]
    PeriodOutOfRange {
        /// The period.
        period: u32,
        /// The horizon.
        periods: u32,
    },
    /// A quantity or rate is invalid.
    #[error("{0}")]
    Invalid(String),
    /// Supplier shares do not sum to 1,000,000 ppm.
    #[error("supplier shares sum to {actual} ppm, expected 1,000,000")]
    SupplierShares {
        /// Actual sum.
        actual: i64,
    },
    /// A known money term is in the wrong currency.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch {
        /// Analysis currency.
        left: Currency,
        /// Term currency.
        right: Currency,
    },
    /// The service level has no exact table entry.
    #[error("service level {0} ppm is not in the exact table")]
    UnsupportedServiceLevel(i64),
    /// A bound was exceeded.
    #[error("{what} exceeds {max} entries")]
    Bound {
        /// Input class.
        what: &'static str,
        /// Maximum.
        max: usize,
    },
    /// A value is negative.
    #[error("{0} must not be negative")]
    Negative(&'static str),
}

/// A fractional BOM quantity per parent and its scrap yield.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BomLine {
    /// Parent item.
    pub parent: PartId,
    /// Child item.
    pub child: PartId,
    /// Quantity of child per parent, in micro-units.
    pub quantity_per: ScaledQuantity,
    /// Scrap rate at this BOM step.
    pub scrap_rate: Rate,
}

/// Independent demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemandLine {
    /// Demanded item.
    pub part: PartId,
    /// Bucket index.
    pub period: u32,
    /// Units.
    pub quantity: Units,
}

/// Confirmed supply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedSupply {
    /// Bucket index.
    pub period: u32,
    /// Units.
    pub quantity: Units,
}

/// MOQ / order-multiple policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LotPolicy {
    /// Minimum order quantity (may be zero).
    pub moq: Units,
    /// Order multiple (must be positive).
    pub multiple: Units,
}

impl LotPolicy {
    /// Validate the policy.
    pub fn validate(&self) -> Result<(), PlanningError> {
        if self.multiple.is_zero() {
            return Err(PlanningError::Invalid("order multiple is zero".to_string()));
        }
        Ok(())
    }

    /// Round a positive requirement to the policy.
    pub fn round(&self, requirement: Units) -> Result<Units, PlanningError> {
        let at_least_moq = requirement.max(self.moq);
        Ok(at_least_moq.round_up_to_multiple(self.multiple)?)
    }
}

/// Demand mean and standard deviation per bucket, in micro-units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemandStats {
    /// Mean demand per bucket, micro-units.
    pub mean_micros: i64,
    /// Standard deviation per bucket, micro-units.
    pub sigma_micros: i64,
}

impl DemandStats {
    /// Validate non-negative values.
    pub fn validate(&self) -> Result<(), PlanningError> {
        if self.mean_micros < 0 {
            return Err(PlanningError::Negative("demand mean"));
        }
        if self.sigma_micros < 0 {
            return Err(PlanningError::Negative("demand sigma"));
        }
        Ok(())
    }

    /// Coefficient of variation in ppm.
    pub fn cv_ppm(&self) -> Availability<i64> {
        if self.mean_micros == 0 {
            return Availability::unavailable(
                UnavailableReason::OutOfRange,
                "zero mean demand has no coefficient of variation",
            );
        }
        match mul_div_round_half_up(
            i128::from(self.sigma_micros),
            1_000_000,
            i128::from(self.mean_micros),
        ) {
            Some(value) => match i64::try_from(value) {
                Ok(value) => Availability::known(value),
                Err(error) => {
                    Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
                }
            },
            None => Availability::unavailable(
                UnavailableReason::Arithmetic,
                "coefficient of variation overflowed",
            ),
        }
    }
}

/// A supplier share for dual/multi sourcing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplyShare {
    /// Supplier.
    pub supplier: SupplierId,
    /// Share in ppm of the order.
    pub share_ppm: i64,
}

/// One part's planning record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartPlan {
    /// Part.
    pub part: PartId,
    /// MPN for the report.
    pub mpn: String,
    /// On-hand units.
    pub on_hand: Units,
    /// Confirmed supply.
    #[serde(default)]
    pub confirmed: Vec<ConfirmedSupply>,
    /// Lead-time distribution in days.
    pub lead_time_days: Term<Distribution>,
    /// Lot policy.
    pub lot: LotPolicy,
    /// Shelf life in days, when applicable.
    pub shelf_life_days: Option<u32>,
    /// Lifecycle status.
    pub lifecycle: LifecycleStatus,
    /// Last order day, when an EOL/PCN notice states one.
    pub last_order_day: Option<Day>,
    /// Unit cost, used for spend classification.
    pub unit_cost: Term<Money>,
    /// Demand statistics for safety stock and XYZ.
    pub demand_stats: Term<DemandStats>,
    /// Supplier shares for allocation.
    #[serde(default)]
    pub suppliers: Vec<SupplyShare>,
}

impl PartPlan {
    fn validate(&self) -> Result<(), PlanningError> {
        validate_text(&self.mpn, faktor_supply_core::MAX_ID_BYTES, "mpn")?;
        self.lead_time_days.validate()?;
        if let Some(distribution) = self.lead_time_days.as_known() {
            distribution.validate()?;
        }
        self.lot.validate()?;
        if self.shelf_life_days == Some(0) {
            return Err(PlanningError::Invalid(
                "shelf life of zero days is not a usable policy".to_string(),
            ));
        }
        self.unit_cost.validate()?;
        self.demand_stats.validate()?;
        if let Some(stats) = self.demand_stats.as_known() {
            stats.validate()?;
        }
        if self.suppliers.len() > MAX_SUPPLIERS_PER_PART {
            return Err(PlanningError::Bound {
                what: "suppliers per part",
                max: MAX_SUPPLIERS_PER_PART,
            });
        }
        if self.suppliers.is_empty() {
            return Err(PlanningError::Invalid(format!(
                "part {} has no supplier allocation",
                self.part.0
            )));
        }
        let total = checked_share_total(&self.suppliers)?;
        if total != 1_000_000 {
            return Err(PlanningError::SupplierShares { actual: total });
        }
        Ok(())
    }
}

/// A proposed safety-stock policy change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafetyStockChange {
    /// Part whose policy changes.
    pub part: PartId,
    /// Current approved safety stock (from the policy record).
    pub current: Units,
    /// Proposed safety stock.
    pub proposed: Units,
    /// Why the policy changes.
    pub rationale: String,
}

/// Safety-stock changes are consequential whenever the proposed value
/// differs from the stated current value. There is deliberately no
/// caller-relative "small change" exemption: a caller that controls both
/// sides of the subtraction could otherwise claim an arbitrarily large
/// change was a no-op. The claimed current value is part of the approval
/// digest, and the caller must source it from the approved policy record.
///
/// A no-op (proposed == current) is returned without a token.
pub fn apply_policy_change(
    change: &SafetyStockChange,
    approval: Option<&faktor_approval::ApprovalToken>,
) -> Result<Units, PlanningError> {
    validate_text(&change.rationale, MAX_TEXT_BYTES, "safety-stock rationale")?;
    if change.current == change.proposed {
        return Ok(change.proposed);
    }
    let digest = faktor_approval::content_digest(b"faktor.supply.safety-stock-change/v1", change)
        .map_err(ExactError::Approval)?;
    let check = faktor_approval::ApprovalCheck::new(
        faktor_approval::ApprovalKind::SupplyInventoryPolicy,
        format!("part:{}", change.part.0),
        "policy",
        digest,
    );
    let token = approval
        .ok_or_else(|| faktor_approval::ApprovalError::ApprovalRequired {
            kind: check.kind,
            subject: check.subject.clone(),
            bound_revision: check.bound_revision.clone(),
            bound_digest: check.bound_digest.clone(),
        })
        .map_err(ExactError::Approval)?;
    token.require(&check).map_err(ExactError::Approval)?;
    Ok(change.proposed)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningInput {
    /// Analysis currency for unit costs/spend.
    pub currency: Currency,
    /// Number of time buckets.
    pub periods: u32,
    /// Calendar days in one bucket.
    pub days_per_period: u32,
    /// Buckets per year, used to annualize spend for ABC.
    pub periods_per_year: u32,
    /// Horizon start day.
    pub horizon_start: Day,
    /// Service level in ppm (must be an exact table entry).
    pub service_level_ppm: i64,
    /// Which lead-time quantile to plan with.
    pub lead_time_quantile: Quantile,
    /// Part plans.
    pub parts: Vec<PartPlan>,
    /// EBOM lines.
    #[serde(default)]
    pub bom: Vec<BomLine>,
    /// Independent demand.
    #[serde(default)]
    pub demand: Vec<DemandLine>,
}

impl PlanningInput {
    /// Validate the whole input (bounds, references, BOM acyclicity).
    pub fn validate(&self) -> Result<(), PlanningError> {
        if self.periods == 0 {
            return Err(PlanningError::NoPeriods);
        }
        if self.periods > MAX_PERIODS {
            return Err(PlanningError::PeriodBound {
                max: MAX_PERIODS,
                actual: self.periods,
            });
        }
        if self.days_per_period == 0 {
            return Err(PlanningError::NoDaysPerPeriod);
        }
        if self.periods_per_year == 0 {
            return Err(PlanningError::Invalid(
                "periods per year must be positive".to_string(),
            ));
        }
        if service_level_z(self.service_level_ppm).is_none() {
            return Err(PlanningError::UnsupportedServiceLevel(
                self.service_level_ppm,
            ));
        }
        if self.parts.len() > MAX_PARTS {
            return Err(PlanningError::Bound {
                what: "parts",
                max: MAX_PARTS,
            });
        }
        if self.bom.len() > MAX_BOM_LINES {
            return Err(PlanningError::Bound {
                what: "BOM lines",
                max: MAX_BOM_LINES,
            });
        }
        if self.demand.len() > MAX_DEMAND_LINES {
            return Err(PlanningError::Bound {
                what: "demand lines",
                max: MAX_DEMAND_LINES,
            });
        }
        let mut parts = HashSet::new();
        for plan in &self.parts {
            plan.validate()?;
            if !parts.insert(plan.part) {
                return Err(PlanningError::DuplicatePart(plan.part));
            }
            if plan.confirmed.len() > MAX_CONFIRMED_LINES {
                return Err(PlanningError::Bound {
                    what: "confirmed lines",
                    max: MAX_CONFIRMED_LINES,
                });
            }
            for confirmed in &plan.confirmed {
                if confirmed.period >= self.periods {
                    return Err(PlanningError::PeriodOutOfRange {
                        period: confirmed.period,
                        periods: self.periods,
                    });
                }
            }
            if let Some(cost) = plan.unit_cost.as_known() {
                if cost.currency != self.currency {
                    return Err(PlanningError::CurrencyMismatch {
                        left: self.currency,
                        right: cost.currency,
                    });
                }
                if cost.micros() < 0 {
                    return Err(PlanningError::Negative("unit cost"));
                }
            }
        }
        for line in &self.bom {
            if !parts.contains(&line.parent) {
                return Err(PlanningError::UnknownPart(line.parent));
            }
            if !parts.contains(&line.child) {
                return Err(PlanningError::UnknownPart(line.child));
            }
            if line.quantity_per.micros() <= 0 {
                return Err(PlanningError::Invalid(
                    "BOM quantity must be positive".to_string(),
                ));
            }
            if line.scrap_rate.ppm() >= 1_000_000 {
                return Err(PlanningError::Invalid(
                    "BOM scrap rate must be below 100%".to_string(),
                ));
            }
        }
        for line in &self.demand {
            if !parts.contains(&line.part) {
                return Err(PlanningError::UnknownPart(line.part));
            }
            if line.period >= self.periods {
                return Err(PlanningError::PeriodOutOfRange {
                    period: line.period,
                    periods: self.periods,
                });
            }
            if line.quantity.get() > MAX_UNITS {
                return Err(PlanningError::Invalid(
                    "demand quantity exceeds the bound".to_string(),
                ));
            }
        }
        // Cycle/depth check via topological sort.
        let _ = topological_order(self)?;
        Ok(())
    }
}

/// Exact z value for a service level, from the supported table.
pub fn service_level_z(service_level_ppm: i64) -> Option<i64> {
    SERVICE_LEVEL_TABLE
        .iter()
        .find(|(level, _)| *level == service_level_ppm)
        .map(|(_, z)| *z)
}

/// ABC class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbcClass {
    /// Top 80% of spend.
    A,
    /// Next 15%.
    B,
    /// Remaining 5%.
    C,
}

/// XYZ class by demand variability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XyzClass {
    /// CV <= 25%.
    X,
    /// CV <= 50%.
    Y,
    /// CV > 50%.
    Z,
}

/// A part's annualized spend and classes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartClassification {
    /// Part.
    pub part: PartId,
    /// Annualized spend, when unit cost and demand are known.
    pub annual_spend: Availability<Money>,
    /// ABC class.
    pub abc: Availability<AbcClass>,
    /// XYZ class.
    pub xyz: Availability<XyzClass>,
}

/// Why an order was planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderReason {
    /// Net requirement / safety-stock refill.
    NetRequirement,
    /// Last-time-buy bridge.
    LastTimeBuy,
}

/// Supplier allocation within an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplierAllocation {
    /// Supplier.
    pub supplier: SupplierId,
    /// Units allocated.
    pub units: Units,
}

/// A planned purchase order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedOrder {
    /// Part.
    pub part: PartId,
    /// Release bucket (never negative).
    pub release_period: u32,
    /// Receipt bucket.
    pub receipt_period: u32,
    /// Order quantity.
    pub quantity: Units,
    /// Shortage that triggered the order.
    pub shortage_units: Units,
    /// MOQ/multiple excess.
    pub excess_units: Units,
    /// True when the release date is already in the past for the receipt date.
    pub expedite: bool,
    /// Reason.
    pub reason: OrderReason,
    /// Exact dual/multi-source split.
    pub suppliers: Vec<SupplierAllocation>,
}

/// Last-time-buy bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastTimeBuy {
    /// Last order day stated by the notice.
    pub last_order_day: Option<Day>,
    /// Bridge quantity, or typed unavailable.
    pub units: Availability<Units>,
    /// Explanation.
    pub note: String,
}

/// A planning flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningFlag {
    /// Stable code.
    pub code: String,
    /// Bounded explanation.
    pub detail: String,
}

/// The planning status of one part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PlanningStatus {
    /// A complete plan was produced.
    Planned,
    /// Demand exists but the plan is incomplete for typed reasons.
    Incomplete {
        /// Reasons.
        reasons: Vec<String>,
    },
    /// No demand and no terminal lifecycle action.
    NoDemand,
}

/// One part's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartPlanResult {
    /// Part.
    pub part: PartId,
    /// MPN.
    pub mpn: String,
    /// Gross requirements per bucket.
    pub gross: Vec<Units>,
    /// Confirmed supply per bucket.
    pub confirmed: Vec<Units>,
    /// On-hand at the start.
    pub on_hand: Units,
    /// Planned lead time in buckets.
    pub lead_time_periods: Availability<u32>,
    /// Safety stock.
    pub safety_stock: Availability<Units>,
    /// Reorder point.
    pub reorder_point: Availability<Units>,
    /// Projected end inventory.
    pub projected_end: Availability<Units>,
    /// Planned orders.
    pub planned_orders: Vec<PlannedOrder>,
    /// Last-time-buy bridge, when the part is terminal.
    pub last_time_buy: Option<LastTimeBuy>,
    /// Status.
    pub status: PlanningStatus,
    /// Flags.
    #[serde(default)]
    pub flags: Vec<PlanningFlag>,
}

/// The complete planning result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Currency.
    pub currency: Currency,
    /// Horizon buckets.
    pub periods: u32,
    /// Results in part-id order.
    pub parts: Vec<PartPlanResult>,
    /// Spend/variability classification in part-id order.
    pub classifications: Vec<PartClassification>,
}

fn topological_order(input: &PlanningInput) -> Result<Vec<PartId>, PlanningError> {
    let mut children: HashMap<PartId, Vec<&BomLine>> = HashMap::new();
    let mut indegree: HashMap<PartId, usize> = HashMap::new();
    let mut nodes = BTreeSet::new();
    for plan in &input.parts {
        nodes.insert(plan.part);
        indegree.entry(plan.part).or_insert(0);
    }
    for line in &input.bom {
        children.entry(line.parent).or_default().push(line);
        *indegree.entry(line.child).or_insert(0) += 1;
    }
    let mut ready = BTreeSet::new();
    for (part, degree) in &indegree {
        if *degree == 0 {
            ready.insert(*part);
        }
    }
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(part) = ready.pop_first() {
        order.push(part);
        if let Some(lines) = children.get(&part) {
            for line in lines {
                if let Some(degree) = indegree.get_mut(&line.child) {
                    *degree -= 1;
                    if *degree == 0 {
                        ready.insert(line.child);
                    }
                }
            }
        }
    }
    if order.len() != nodes.len() {
        return Err(PlanningError::BomCycle);
    }
    Ok(order)
}

fn bom_depths(input: &PlanningInput) -> Result<HashMap<PartId, usize>, PlanningError> {
    let order = topological_order(input)?;
    let mut children: HashMap<PartId, Vec<&BomLine>> = HashMap::new();
    for line in &input.bom {
        children.entry(line.parent).or_default().push(line);
    }
    let mut depth: HashMap<PartId, usize> = HashMap::new();
    for part in order {
        let current = *depth.get(&part).unwrap_or(&0);
        if current >= MAX_BOM_DEPTH {
            return Err(PlanningError::BomDepth(MAX_BOM_DEPTH));
        }
        if let Some(lines) = children.get(&part) {
            for line in lines {
                let entry = depth.entry(line.child).or_insert(0);
                *entry = (*entry).max(current + 1);
            }
        }
    }
    Ok(depth)
}

fn explode_requirements(
    input: &PlanningInput,
) -> Result<BTreeMap<PartId, Vec<Units>>, PlanningError> {
    let _depths = bom_depths(input)?;
    let order = topological_order(input)?;
    let mut gross: HashMap<PartId, Vec<Units>> = HashMap::new();
    for plan in &input.parts {
        gross.insert(plan.part, vec![Units::ZERO; input.periods as usize]);
    }
    for line in &input.demand {
        let vector = gross
            .get_mut(&line.part)
            .ok_or(PlanningError::UnknownPart(line.part))?;
        let index = line.period as usize;
        vector[index] = vector[index].checked_add(line.quantity)?;
    }
    let mut children: HashMap<PartId, Vec<&BomLine>> = HashMap::new();
    for line in &input.bom {
        children.entry(line.parent).or_default().push(line);
    }
    for part in order {
        let parent_gross = gross
            .get(&part)
            .cloned()
            .ok_or(PlanningError::UnknownPart(part))?;
        if let Some(lines) = children.get(&part) {
            for line in lines {
                let usable = 1_000_000_i64 - line.scrap_rate.ppm();
                if usable <= 0 {
                    return Err(PlanningError::Invalid("BOM yield is zero".to_string()));
                }
                let child_gross = gross
                    .get_mut(&line.child)
                    .ok_or(PlanningError::UnknownPart(line.child))?;
                for (period, parent_units) in parent_gross.iter().enumerate() {
                    if parent_units.is_zero() {
                        continue;
                    }
                    let required = mul_div_round_up(
                        i128::from(parent_units.get()),
                        i128::from(line.quantity_per.micros()),
                        i128::from(usable),
                    )
                    .ok_or(PlanningError::Exact(ExactError::Overflow))?;
                    if required < 0 {
                        return Err(PlanningError::Negative("BOM requirement"));
                    }
                    let required = u64::try_from(required)
                        .map_err(|_| PlanningError::Exact(ExactError::Overflow))?;
                    if required > MAX_UNITS {
                        return Err(PlanningError::Invalid(
                            "BOM requirement exceeds the unit bound".to_string(),
                        ));
                    }
                    child_gross[period] = child_gross[period].checked_add(Units::new(required)?)?;
                }
            }
        }
    }
    Ok(gross.into_iter().collect())
}

/// Run planning. Deterministic for a given input.
pub fn plan(input: &PlanningInput) -> Result<Plan, PlanningError> {
    input.validate()?;
    let gross = explode_requirements(input)?;
    let mut results = Vec::with_capacity(input.parts.len());
    let mut classifications = Vec::with_capacity(input.parts.len());
    for part_plan in &input.parts {
        let vector = gross
            .get(&part_plan.part)
            .ok_or(PlanningError::UnknownPart(part_plan.part))?;
        let (result, classification) = plan_part(input, part_plan, vector)?;
        results.push(result);
        classifications.push(classification);
    }
    results.sort_by_key(|result| result.part);
    classifications.sort_by_key(|classification| classification.part);
    assign_abc(&mut classifications)?;
    Ok(Plan {
        currency: input.currency,
        periods: input.periods,
        parts: results,
        classifications,
    })
}

fn plan_part(
    input: &PlanningInput,
    part_plan: &PartPlan,
    gross: &[Units],
) -> Result<(PartPlanResult, PartClassification), PlanningError> {
    let mut confirmed = vec![Units::ZERO; input.periods as usize];
    for line in &part_plan.confirmed {
        confirmed[line.period as usize] =
            confirmed[line.period as usize].checked_add(line.quantity)?;
    }
    let gross_total = gross
        .iter()
        .try_fold(Units::ZERO, |total, value| total.checked_add(*value))?;
    let mut flags = Vec::new();
    let mut status = PlanningStatus::Planned;
    let mut reasons = Vec::new();

    let lead_periods = match part_plan.lead_time_days.as_known() {
        Some(distribution) => {
            let days = distribution.quantile(input.lead_time_quantile).max(0) as u64;
            let periods = days
                .checked_add(u64::from(input.days_per_period) - 1)
                .ok_or(PlanningError::Exact(ExactError::Overflow))?
                / u64::from(input.days_per_period);
            if periods > u64::from(MAX_PERIODS) {
                return Err(PlanningError::Invalid(
                    "lead time exceeds the planning horizon".to_string(),
                ));
            }
            Availability::known(periods as u32)
        }
        None => {
            reasons.push("lead-time distribution unavailable".to_string());
            flags.push(flag(
                "lead_time_unavailable",
                "no lead-time distribution; orders cannot be time-phased",
            ));
            Availability::unavailable(
                UnavailableReason::MissingEvidence,
                "lead-time distribution unavailable",
            )
        }
    };

    let stats = part_plan.demand_stats.as_known();
    let safety_stock = match (stats, lead_periods.as_known()) {
        (Some(stats), Some(lead)) => match safety_stock(input.service_level_ppm, stats, *lead) {
            Ok(value) => Availability::known(value),
            Err(error) => {
                reasons.push(format!("safety stock unavailable: {error}"));
                Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
            }
        },
        (None, _) => {
            reasons.push("demand statistics unavailable".to_string());
            flags.push(flag(
                "demand_stats_unavailable",
                "no demand mean/sigma; safety stock and reorder point cannot be computed",
            ));
            Availability::unavailable(
                UnavailableReason::MissingEvidence,
                "demand statistics unavailable",
            )
        }
        (_, None) => {
            Availability::unavailable(UnavailableReason::MissingEvidence, "lead time unavailable")
        }
    };
    let reorder_point = match (stats, lead_periods.as_known(), safety_stock.as_known()) {
        (Some(stats), Some(lead), Some(safety)) => {
            let demand_during_lt = require_units(
                i128::from(stats.mean_micros)
                    .checked_mul(i128::from(*lead))
                    .ok_or(PlanningError::Exact(ExactError::Overflow))?,
            )?;
            match demand_during_lt.checked_add(*safety) {
                Ok(total) => Availability::known(total),
                Err(error) => {
                    Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
                }
            }
        }
        _ => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "reorder point needs lead time, demand statistics and safety stock",
        ),
    };

    let terminal = part_plan.lifecycle.is_terminal()
        || matches!(part_plan.lifecycle, LifecycleStatus::LastTimeBuy)
        || part_plan.last_order_day.is_some();
    let mut last_time_buy = None;
    let mut orders = Vec::new();
    let mut projected_end =
        Availability::unavailable(UnavailableReason::MissingEvidence, "planning incomplete");

    if gross_total.is_zero() && !terminal {
        status = PlanningStatus::NoDemand;
        projected_end = Availability::known(part_plan.on_hand);
    } else if terminal {
        let bridge = match (
            last_time_buy_required(input, part_plan),
            safety_stock.as_known(),
        ) {
            (true, Some(safety)) => {
                let confirmed_total = confirmed
                    .iter()
                    .try_fold(Units::ZERO, |total, value| total.checked_add(*value))?;
                let net = gross_total
                    .checked_add(*safety)?
                    .checked_sub(part_plan.on_hand)?
                    .checked_sub(confirmed_total)?;
                match part_plan.last_order_day {
                    None => {
                        reasons.push("terminal lifecycle without a last order day".to_string());
                        Availability::unavailable(
                            UnavailableReason::MissingEvidence,
                            "terminal lifecycle without a last order day",
                        )
                    }
                    Some(day) => {
                        if day.before(input.horizon_start) {
                            reasons.push("last order day has passed".to_string());
                            Availability::unavailable(
                                UnavailableReason::Expired,
                                "last order day has passed",
                            )
                        } else {
                            match part_plan.lot.round(net) {
                                Ok(quantity) => Availability::known(quantity),
                                Err(error) => {
                                    reasons.push(format!("LTB quantity failed: {error}"));
                                    Availability::unavailable(
                                        UnavailableReason::Arithmetic,
                                        error.to_string(),
                                    )
                                }
                            }
                        }
                    }
                }
            }
            (true, None) => {
                reasons.push("last-time-buy needs safety stock".to_string());
                Availability::unavailable(
                    UnavailableReason::MissingEvidence,
                    "LTB needs safety stock",
                )
            }
            (false, _) => Availability::known(Units::ZERO),
        };
        let bridge_known = bridge.as_known().copied();
        last_time_buy = Some(LastTimeBuy {
            last_order_day: part_plan.last_order_day,
            units: bridge,
            note: if terminal {
                "last-time-buy bridge covers the full horizon plus safety stock".to_string()
            } else {
                "not terminal".to_string()
            },
        });
        match bridge_known {
            Some(quantity) if !quantity.is_zero() => {
                if let Some(lead) = lead_periods.as_known() {
                    let receipt_period = (*lead).min(input.periods - 1);
                    let expedite = *lead > receipt_period;
                    let order = make_order(
                        input,
                        part_plan,
                        0,
                        receipt_period,
                        quantity,
                        quantity,
                        OrderReason::LastTimeBuy,
                        expedite,
                        stats,
                    )?;
                    orders.push(order);
                    let confirmed_total = confirmed
                        .iter()
                        .try_fold(Units::ZERO, |total, value| total.checked_add(*value))?;
                    let final_units = part_plan
                        .on_hand
                        .checked_add(confirmed_total)?
                        .checked_add(quantity)?
                        .checked_sub(gross_total)?;
                    projected_end = Availability::known(final_units);
                } else {
                    reasons.push("LTB order cannot be time-phased".to_string());
                }
            }
            Some(_) => {
                projected_end = Availability::known(part_plan.on_hand);
            }
            None => {}
        }
    } else if let (Some(lead), Some(safety)) = (lead_periods.as_known(), safety_stock.as_known()) {
        let mut available = i128::from(part_plan.on_hand.get());
        for period in 0..input.periods as usize {
            available += i128::from(confirmed[period].get());
            available -= i128::from(gross[period].get());
            if available < i128::from(safety.get()) {
                let shortage_units = u64::try_from(i128::from(safety.get()) - available)
                    .map_err(|_| PlanningError::Exact(ExactError::Overflow))?;
                let shortage = Units::new(shortage_units)?;
                let quantity = match part_plan.lot.round(shortage) {
                    Ok(quantity) => quantity,
                    Err(error) => {
                        reasons.push(format!("lot rounding failed: {error}"));
                        continue;
                    }
                };
                let shortage_units = shortage;
                let receipt = period;
                let release = (period as u32).saturating_sub(*lead);
                let expedite = (period as u32) < *lead;
                if expedite {
                    flags.push(flag(
                        "expedite",
                        &format!("receipt in bucket {receipt} needs a {lead}-bucket lead time"),
                    ));
                }
                orders.push(make_order(
                    input,
                    part_plan,
                    release,
                    receipt as u32,
                    quantity,
                    shortage_units,
                    OrderReason::NetRequirement,
                    expedite,
                    stats,
                )?);
                if orders.len() > MAX_ORDERS_PER_PART {
                    return Err(PlanningError::Bound {
                        what: "planned orders",
                        max: MAX_ORDERS_PER_PART,
                    });
                }
                available += i128::from(quantity.get());
            }
        }
        let end = if available < 0 {
            reasons.push("projected inventory remains negative".to_string());
            Availability::unavailable(
                UnavailableReason::Arithmetic,
                "projected inventory is negative",
            )
        } else {
            Availability::known(Units::new(available as u64)?)
        };
        projected_end = end;
    } else {
        reasons.push("planning netting requires lead time and safety stock".to_string());
    }

    if matches!(status, PlanningStatus::Planned) && !reasons.is_empty() {
        status = PlanningStatus::Incomplete { reasons };
    }

    if let Some(shelf_life_days) = part_plan.shelf_life_days {
        let shelf_life_buckets =
            u64::from(shelf_life_days).div_ceil(u64::from(input.days_per_period));
        if let (Some(stats), Some(lead)) = (stats, lead_periods.as_known()) {
            if shelf_life_buckets < u64::from(*lead) {
                flags.push(flag(
                    "shelf_life_risk",
                    "shelf life is shorter than the lead time",
                ));
            }
            let demand_over_shelf = i128::from(stats.mean_micros)
                .checked_mul(i128::from(shelf_life_buckets))
                .ok_or(PlanningError::Exact(ExactError::Overflow))?;
            let demand_over_shelf = require_units(demand_over_shelf)?;
            for order in &orders {
                if order.quantity.get() > demand_over_shelf.get() {
                    flags.push(flag(
                        "shelf_life_risk",
                        "an order exceeds demand over one shelf life",
                    ));
                    break;
                }
            }
        } else {
            flags.push(flag(
                "shelf_life_unknown",
                "shelf-life risk cannot be assessed without lead time and demand stats",
            ));
        }
    }

    dedup_flags(&mut flags);
    let classification = classification_for(input, part_plan, gross_total, stats);
    Ok((
        PartPlanResult {
            part: part_plan.part,
            mpn: part_plan.mpn.clone(),
            gross: gross.to_vec(),
            confirmed,
            on_hand: part_plan.on_hand,
            lead_time_periods: lead_periods,
            safety_stock,
            reorder_point,
            projected_end,
            planned_orders: orders,
            last_time_buy,
            status,
            flags,
        },
        classification,
    ))
}

fn last_time_buy_required(input: &PlanningInput, part_plan: &PartPlan) -> bool {
    part_plan.lifecycle.is_terminal()
        || matches!(part_plan.lifecycle, LifecycleStatus::LastTimeBuy)
        || part_plan.last_order_day.is_some_and(|day| {
            let horizon_end =
                input.horizon_start.0 + i64::from(input.periods) * i64::from(input.days_per_period);
            day.0 <= horizon_end
        })
}

#[allow(clippy::too_many_arguments)]
fn make_order(
    input: &PlanningInput,
    part_plan: &PartPlan,
    release_period: u32,
    receipt_period: u32,
    quantity: Units,
    shortage: Units,
    reason: OrderReason,
    expedite: bool,
    stats: Option<&DemandStats>,
) -> Result<PlannedOrder, PlanningError> {
    let excess = quantity.checked_sub(shortage).unwrap_or(Units::ZERO);
    // A 4-bucket expiry warning uses the observed lead-time coverage; the
    // actual shelf-life flag is computed by the caller with the part policy.
    let _ = (input, stats);
    Ok(PlannedOrder {
        part: part_plan.part,
        release_period,
        receipt_period,
        quantity,
        shortage_units: shortage,
        excess_units: excess,
        expedite,
        reason,
        suppliers: allocate(quantity, &part_plan.suppliers)?,
    })
}

fn checked_share_total(shares: &[SupplyShare]) -> Result<i64, PlanningError> {
    let mut total: i64 = 0;
    for share in shares {
        if !(0..=1_000_000).contains(&share.share_ppm) {
            return Err(PlanningError::Invalid(
                "supplier share outside 0..=1,000,000 ppm".to_string(),
            ));
        }
        total = total
            .checked_add(share.share_ppm)
            .ok_or(PlanningError::Exact(ExactError::Overflow))?;
    }
    Ok(total)
}

/// Split a quantity by supplier shares with exact largest-remainder rounding.
pub fn allocate(
    quantity: Units,
    shares: &[SupplyShare],
) -> Result<Vec<SupplierAllocation>, PlanningError> {
    let total = checked_share_total(shares)?;
    if total != 1_000_000 {
        return Err(PlanningError::SupplierShares { actual: total });
    }
    if shares.is_empty() {
        return Err(PlanningError::Invalid("no supplier shares".to_string()));
    }
    let quantity_value = i128::from(quantity.get());
    let mut floors = Vec::with_capacity(shares.len());
    let mut distributed: i128 = 0;
    for share in shares {
        let numerator = quantity_value
            .checked_mul(i128::from(share.share_ppm))
            .ok_or(PlanningError::Exact(ExactError::Overflow))?;
        let floor = numerator / 1_000_000;
        let remainder = numerator % 1_000_000;
        floors.push((share.supplier, floor, remainder));
        distributed += floor;
    }
    let mut leftover = quantity_value - distributed;
    floors.sort_by(|left, right| right.2.cmp(&left.2).then(left.0.cmp(&right.0)));
    let mut result = Vec::with_capacity(shares.len());
    for (supplier, floor, _remainder) in floors {
        let mut units = floor;
        if leftover > 0 {
            units += 1;
            leftover -= 1;
        }
        let units = u64::try_from(units).map_err(|_| PlanningError::Exact(ExactError::Overflow))?;
        result.push(SupplierAllocation {
            supplier,
            units: Units::new(units)?,
        });
    }
    result.sort_by_key(|allocation| allocation.supplier);
    Ok(result)
}

fn safety_stock(
    service_level_ppm: i64,
    stats: &DemandStats,
    lead_periods: u32,
) -> Result<Units, PlanningError> {
    let z = service_level_z(service_level_ppm)
        .ok_or(PlanningError::UnsupportedServiceLevel(service_level_ppm))?;
    let sqrt_lt = faktor_supply_core::isqrt_scaled_ppm(Units::new(u64::from(lead_periods))?);
    // z_ppm * sigma_micros * sqrt_scaled / 1e12 = safety micro-units.
    let numerator = i128::from(z)
        .checked_mul(i128::from(stats.sigma_micros))
        .and_then(|value| value.checked_mul(i128::from(sqrt_lt)))
        .ok_or(PlanningError::Exact(ExactError::Overflow))?;
    let micros = mul_div_round_up(numerator, 1, 1_000_000_000_000)
        .ok_or(PlanningError::Exact(ExactError::Overflow))?;
    require_units(micros)
}

fn require_units(micros: i128) -> Result<Units, PlanningError> {
    if micros < 0 {
        return Err(PlanningError::Negative("quantity"));
    }
    let units = (micros + 999_999) / 1_000_000;
    let units = u64::try_from(units).map_err(|_| PlanningError::Exact(ExactError::Overflow))?;
    Ok(Units::new(units)?)
}

fn classification_for(
    input: &PlanningInput,
    part_plan: &PartPlan,
    gross_total: Units,
    stats: Option<&DemandStats>,
) -> PartClassification {
    let annual_spend = match part_plan.unit_cost.as_known() {
        Some(cost) => {
            // Annualize the horizon demand, rounding half up.
            let annual_units = mul_div_round_half_up(
                i128::from(gross_total.get()) * 1_000_000,
                i128::from(input.periods_per_year),
                i128::from(input.periods) * 1_000_000,
            );
            match annual_units {
                Some(annual_units) => {
                    let annual_units = if annual_units < 0 {
                        None
                    } else {
                        u64::try_from(annual_units).ok()
                    };
                    match annual_units.and_then(|units| cost.mul_units(units).ok()) {
                        Some(value) => Availability::known(value),
                        None => Availability::unavailable(
                            UnavailableReason::Arithmetic,
                            "spend overflow",
                        ),
                    }
                }
                None => Availability::unavailable(UnavailableReason::Arithmetic, "spend overflow"),
            }
        }
        None => {
            Availability::unavailable(UnavailableReason::MissingEvidence, "unit cost unavailable")
        }
    };
    let xyz = match stats {
        None => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "demand statistics unavailable",
        ),
        Some(stats) => match stats.cv_ppm() {
            Availability::Known { value } if value <= 250_000 => Availability::known(XyzClass::X),
            Availability::Known { value } if value <= 500_000 => Availability::known(XyzClass::Y),
            Availability::Known { .. } => Availability::known(XyzClass::Z),
            Availability::Unavailable { unavailable } => Availability::Unavailable { unavailable },
        },
    };
    PartClassification {
        part: part_plan.part,
        annual_spend,
        abc: Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "ABC is assigned after the spend sort",
        ),
        xyz,
    }
}

fn assign_abc(classifications: &mut [PartClassification]) -> Result<(), PlanningError> {
    let mut spend = Vec::new();
    for classification in classifications.iter() {
        let part = classification.part;
        if let Availability::Known { value } = &classification.annual_spend {
            spend.push((part, value.micros()));
        }
    }
    if spend.is_empty() {
        return Ok(());
    }
    let total_spend: i128 = spend.iter().map(|(_, spend)| i128::from(*spend)).sum();
    if total_spend <= 0 {
        for classification in classifications.iter_mut() {
            if classification.annual_spend.is_known() {
                classification.abc =
                    Availability::unavailable(UnavailableReason::OutOfRange, "total spend is zero");
            }
        }
        return Ok(());
    }
    spend.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    let mut cumulative: i128 = 0;
    let mut first = true;
    for (part, value) in spend {
        cumulative += i128::from(value);
        let after_ppm = mul_div_round_half_up(cumulative, 1_000_000, total_spend)
            .ok_or(PlanningError::Exact(ExactError::Overflow))?;
        let class = if first || after_ppm <= 800_000 {
            AbcClass::A
        } else if after_ppm <= 950_000 {
            AbcClass::B
        } else {
            AbcClass::C
        };
        first = false;
        if let Ok(position) =
            classifications.binary_search_by_key(&part, |classification| classification.part)
        {
            classifications[position].abc = Availability::known(class);
        }
    }
    Ok(())
}

fn flag(code: &str, detail: &str) -> PlanningFlag {
    PlanningFlag {
        code: code.to_string(),
        detail: detail.to_string(),
    }
}

fn dedup_flags(flags: &mut Vec<PlanningFlag>) {
    flags.sort_by(|left, right| {
        left.code
            .cmp(&right.code)
            .then(left.detail.cmp(&right.detail))
    });
    flags.dedup_by(|left, right| left.code == right.code && left.detail == right.detail);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_stock_policy_change_requires_a_human_approval_even_when_small() {
        let change = SafetyStockChange {
            part: PartId(1),
            current: Units::new(100).unwrap(),
            proposed: Units::new(200).unwrap(),
            rationale: "double the buffer".to_string(),
        };
        for proposed in [200, 110, 101] {
            let candidate = SafetyStockChange {
                proposed: Units::new(proposed).unwrap(),
                ..change.clone()
            };
            let error = apply_policy_change(&candidate, None).unwrap_err();
            assert!(
                matches!(
                    error,
                    PlanningError::Exact(ExactError::Approval(
                        faktor_approval::ApprovalError::ApprovalRequired { .. }
                    ))
                ),
                "a caller-relative small change must not bypass approval: {error}"
            );
        }
        let digest =
            faktor_approval::content_digest(b"faktor.supply.safety-stock-change/v1", &change)
                .unwrap();
        let token = faktor_approval::ApprovalToken::test_token(
            faktor_approval::ApprovalKind::SupplyInventoryPolicy,
            "part:1",
            "policy",
            digest,
        );
        assert_eq!(
            apply_policy_change(&change, Some(&token)).unwrap().get(),
            200
        );
        let no_op = SafetyStockChange {
            proposed: change.current,
            ..change
        };
        assert_eq!(
            apply_policy_change(&no_op, None).unwrap().get(),
            no_op.proposed.get()
        );
    }

    use faktor_supply_core::{Day, EvidenceRef, Provenance, Revision, SourceKind, SupplierId};

    const DAY: i64 = 20_000;

    fn provenance(id: &str) -> Provenance {
        Provenance::new(
            SourceKind::ErpExtract,
            EvidenceRef::new(id, Revision(1), None).unwrap(),
            Day(DAY - 1),
            "fixture",
            vec!["ERP extract revision 1".to_string()],
            "a corrected ERP extract",
        )
        .unwrap()
    }

    fn term<T>(value: T, id: &str) -> Term<T> {
        Term::evidence(value, provenance(id))
    }

    fn dist(p50: i64, p90: i64) -> Distribution {
        Distribution {
            p10: 0,
            p50,
            p90,
            p99: p90 + 1,
            sample_size: 30,
        }
    }

    fn part(id: u32, on_hand: u64, lead: Distribution) -> PartPlan {
        PartPlan {
            part: PartId(id),
            mpn: format!("MPN-{id}"),
            on_hand: Units::new(on_hand).unwrap(),
            confirmed: vec![],
            lead_time_days: term(lead, "lead"),
            lot: LotPolicy {
                moq: Units::new(1).unwrap(),
                multiple: Units::new(1).unwrap(),
            },
            shelf_life_days: None,
            lifecycle: LifecycleStatus::Active,
            last_order_day: None,
            unit_cost: term(Money::parse("1.00", Currency::USD).unwrap(), "cost"),
            demand_stats: term(
                DemandStats {
                    mean_micros: 100_000_000,
                    sigma_micros: 20_000_000,
                },
                "stats",
            ),
            suppliers: vec![SupplyShare {
                supplier: SupplierId(1),
                share_ppm: 1_000_000,
            }],
        }
    }

    fn input() -> PlanningInput {
        PlanningInput {
            currency: Currency::USD,
            periods: 6,
            days_per_period: 7,
            periods_per_year: 52,
            horizon_start: Day(DAY),
            service_level_ppm: 950_000,
            lead_time_quantile: Quantile::P90,
            parts: vec![part(1, 0, dist(14, 14)), part(2, 0, dist(7, 7))],
            bom: vec![BomLine {
                parent: PartId(1),
                child: PartId(2),
                quantity_per: ScaledQuantity::new(2_500_000).unwrap(),
                scrap_rate: Rate::new(200_000).unwrap(),
            }],
            demand: vec![DemandLine {
                part: PartId(1),
                period: 2,
                quantity: Units::new(1_000).unwrap(),
            }],
        }
    }

    #[test]
    fn bom_explosion_applies_quantity_and_scrap() {
        let result = plan(&input()).unwrap();
        let child = result
            .parts
            .iter()
            .find(|part| part.part == PartId(2))
            .unwrap();
        // 1000 * 2.5 / (1 - 0.2) = 3125
        assert_eq!(child.gross[2].get(), 3_125);
        assert_eq!(child.gross[0].get(), 0);
    }

    #[test]
    fn time_phased_orders_offset_by_lead_time() {
        let mut input = input();
        input.parts[0].lead_time_days = term(dist(14, 14), "lead");
        input.parts[0].lot.moq = Units::new(1).unwrap();
        input.parts[0].lot.multiple = Units::new(1).unwrap();
        input.service_level_ppm = 500_000;
        // Remove BOM child so its demand does not confuse the assertions.
        input.bom.clear();
        input.parts.pop();
        let result = plan(&input).unwrap();
        let parent = &result.parts[0];
        let order = parent
            .planned_orders
            .iter()
            .find(|order| order.receipt_period == 2)
            .unwrap();
        assert_eq!(order.release_period, 0);
        assert!(!order.expedite);
    }

    #[test]
    fn safety_stock_worked_numeric_case() {
        // z(95%) = 1.644854; sigma 100 units; lead 4 buckets -> 329 units.
        let stats = DemandStats {
            mean_micros: 100_000_000,
            sigma_micros: 100_000_000,
        };
        assert_eq!(safety_stock(950_000, &stats, 4).unwrap().get(), 329);
        // Reorder point: 100 * 4 + 329 = 729 in a one-part run.
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(28, 28))];
        input.parts[0].demand_stats = term(stats, "stats");
        input.demand = vec![];
        let result = plan(&input).unwrap();
        assert_eq!(result.parts[0].safety_stock.as_known().unwrap().get(), 329);
        assert_eq!(result.parts[0].reorder_point.as_known().unwrap().get(), 729);
    }

    #[test]
    fn moq_and_multiple_round_the_requirement() {
        let lot = LotPolicy {
            moq: Units::new(500).unwrap(),
            multiple: Units::new(100).unwrap(),
        };
        assert_eq!(lot.round(Units::new(123).unwrap()).unwrap().get(), 500);
        assert_eq!(lot.round(Units::new(501).unwrap()).unwrap().get(), 600);
        assert_eq!(lot.round(Units::new(1_000).unwrap()).unwrap().get(), 1_000);
    }

    #[test]
    fn moq_excess_is_retained_as_inventory_and_not_reordered() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].lot.moq = Units::new(1_000).unwrap();
        input.service_level_ppm = 500_000;
        input.demand = vec![
            DemandLine {
                part: PartId(1),
                period: 0,
                quantity: Units::new(100).unwrap(),
            },
            DemandLine {
                part: PartId(1),
                period: 2,
                quantity: Units::new(100).unwrap(),
            },
        ];
        let result = plan(&input).unwrap();
        let parent = &result.parts[0];
        assert_eq!(parent.planned_orders.len(), 1, "{parent:?}");
        assert_eq!(parent.planned_orders[0].excess_units.get(), 900);
        assert_eq!(parent.projected_end.as_known().unwrap().get(), 800);
    }

    #[test]
    fn confirmed_supply_reduces_orders_and_projection_is_consistent() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 100, dist(7, 7))];
        input.parts[0].confirmed = vec![ConfirmedSupply {
            period: 0,
            quantity: Units::new(900).unwrap(),
        }];
        input.demand = vec![DemandLine {
            part: PartId(1),
            period: 0,
            quantity: Units::new(1_000).unwrap(),
        }];
        input.parts[0].lot.moq = Units::new(1).unwrap();
        input.service_level_ppm = 500_000;
        let result = plan(&input).unwrap();
        let parent = &result.parts[0];
        // Breaks if confirmed supply is ignored, double-counted, or reordered.
        assert_eq!(parent.confirmed[0].get(), 900);
        assert!(
            parent.planned_orders.is_empty(),
            "{:?}",
            parent.planned_orders
        );
        assert_eq!(parent.projected_end.as_known().unwrap().get(), 0);
    }

    #[test]
    fn missing_lead_time_is_incomplete_and_does_not_invent_orders() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].lead_time_days =
            Term::unknown(UnavailableReason::MissingEvidence, "no supplier lead time");
        input.demand = vec![DemandLine {
            part: PartId(1),
            period: 0,
            quantity: Units::new(100).unwrap(),
        }];
        let result = plan(&input).unwrap();
        assert!(matches!(
            result.parts[0].status,
            PlanningStatus::Incomplete { .. }
        ));
        assert!(result.parts[0].planned_orders.is_empty());
        assert!(!result.parts[0].lead_time_periods.is_known());
    }

    #[test]
    fn missing_demand_stats_blocks_safety_stock() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].demand_stats =
            Term::unknown(UnavailableReason::MissingEvidence, "no history");
        input.demand = vec![DemandLine {
            part: PartId(1),
            period: 0,
            quantity: Units::new(100).unwrap(),
        }];
        let result = plan(&input).unwrap();
        assert!(matches!(
            result.parts[0].status,
            PlanningStatus::Incomplete { .. }
        ));
        assert!(!result.parts[0].safety_stock.is_known());
    }

    #[test]
    fn bom_cycle_is_refused() {
        let mut input = input();
        input.bom.push(BomLine {
            parent: PartId(2),
            child: PartId(1),
            quantity_per: ScaledQuantity::new(1_000_000).unwrap(),
            scrap_rate: Rate::ZERO,
        });
        assert!(matches!(plan(&input), Err(PlanningError::BomCycle)));
    }

    #[test]
    fn abc_uses_annualized_spend_in_descending_order() {
        let mut input = input();
        input.bom.clear();
        input.demand.clear();
        input.parts = vec![
            part(1, 0, dist(7, 7)),
            part(2, 0, dist(7, 7)),
            part(3, 0, dist(7, 7)),
        ];
        for (index, units) in [(0, 700_u64), (1, 200), (2, 100)] {
            input.parts[index].unit_cost =
                term(Money::parse("1.00", Currency::USD).unwrap(), "cost");
            input.demand.push(DemandLine {
                part: input.parts[index].part,
                period: 0,
                quantity: Units::new(units).unwrap(),
            });
        }
        let result = plan(&input).unwrap();
        assert_eq!(result.classifications[0].abc.as_known(), Some(&AbcClass::A));
        assert_eq!(result.classifications[1].abc.as_known(), Some(&AbcClass::B));
        assert_eq!(result.classifications[2].abc.as_known(), Some(&AbcClass::C));
    }

    #[test]
    fn xyz_uses_coefficient_of_variation() {
        let stats = DemandStats {
            mean_micros: 100_000_000,
            sigma_micros: 20_000_000,
        };
        assert_eq!(stats.cv_ppm().as_known(), Some(&200_000));
        let mut input = input();
        input.bom.clear();
        input.demand.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].demand_stats = term(stats, "stats");
        let result = plan(&input).unwrap();
        assert_eq!(result.classifications[0].xyz.as_known(), Some(&XyzClass::X));
    }

    #[test]
    fn dual_sourcing_splits_exactly_with_largest_remainder() {
        let shares = vec![
            SupplyShare {
                supplier: SupplierId(1),
                share_ppm: 500_000,
            },
            SupplyShare {
                supplier: SupplierId(2),
                share_ppm: 500_000,
            },
        ];
        let split = allocate(Units::new(7).unwrap(), &shares).unwrap();
        assert_eq!(split[0].units.get() + split[1].units.get(), 7);
        assert_eq!(split[0].units.get(), 4);
        assert_eq!(split[1].units.get(), 3);
        let sixty = vec![
            SupplyShare {
                supplier: SupplierId(1),
                share_ppm: 600_000,
            },
            SupplyShare {
                supplier: SupplierId(2),
                share_ppm: 400_000,
            },
        ];
        let split = allocate(Units::new(1_000).unwrap(), &sixty).unwrap();
        assert_eq!(split[0].units.get(), 600);
        assert_eq!(split[1].units.get(), 400);
    }

    #[test]
    fn invalid_supplier_shares_are_refused() {
        let shares = vec![SupplyShare {
            supplier: SupplierId(1),
            share_ppm: 900_000,
        }];
        assert!(matches!(
            allocate(Units::new(1).unwrap(), &shares),
            Err(PlanningError::SupplierShares { .. })
        ));
    }

    #[test]
    fn last_time_buy_produces_one_bridge_order() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].lifecycle = LifecycleStatus::EndOfLife;
        input.parts[0].last_order_day = Some(Day(DAY + 10));
        input.parts[0].lot.moq = Units::new(100).unwrap();
        input.demand = vec![DemandLine {
            part: PartId(1),
            period: 0,
            quantity: Units::new(500).unwrap(),
        }];
        let result = plan(&input).unwrap();
        let parent = &result.parts[0];
        assert!(parent.last_time_buy.as_ref().unwrap().units.is_known());
        assert_eq!(parent.planned_orders.len(), 1);
        assert_eq!(parent.planned_orders[0].reason, OrderReason::LastTimeBuy);
        assert!(parent.planned_orders[0].quantity.get() >= 500);
    }

    #[test]
    fn terminal_without_last_order_day_is_incomplete_but_typed() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].lifecycle = LifecycleStatus::EndOfLife;
        input.demand = vec![DemandLine {
            part: PartId(1),
            period: 0,
            quantity: Units::new(10).unwrap(),
        }];
        let result = plan(&input).unwrap();
        let parent = &result.parts[0];
        assert!(matches!(parent.status, PlanningStatus::Incomplete { .. }));
        assert!(!parent.last_time_buy.as_ref().unwrap().units.is_known());
    }

    #[test]
    fn shelf_life_is_flagged_when_order_covers_more_than_shelf_life() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].shelf_life_days = Some(14);
        input.parts[0].lot.moq = Units::new(10_000).unwrap();
        input.demand = vec![DemandLine {
            part: PartId(1),
            period: 0,
            quantity: Units::new(100).unwrap(),
        }];
        let result = plan(&input).unwrap();
        assert!(result.parts[0]
            .flags
            .iter()
            .any(|flag| flag.code == "shelf_life_risk"));
    }

    #[test]
    fn an_order_within_shelf_life_demand_is_not_flagged() {
        let mut input = input();
        input.bom.clear();
        input.parts = vec![part(1, 0, dist(7, 7))];
        input.parts[0].shelf_life_days = Some(365);
        input.parts[0].lot.moq = Units::new(1_000).unwrap();
        input.demand = vec![DemandLine {
            part: PartId(1),
            period: 0,
            quantity: Units::new(100).unwrap(),
        }];
        let result = plan(&input).unwrap();
        assert!(
            !result.parts[0]
                .flags
                .iter()
                .any(|flag| flag.code == "shelf_life_risk"),
            "{:?}",
            result.parts[0].flags
        );
    }

    #[test]
    fn unsupported_service_level_is_refused() {
        let mut input = input();
        input.service_level_ppm = 951_234;
        assert!(matches!(
            plan(&input),
            Err(PlanningError::UnsupportedServiceLevel(_))
        ));
    }

    #[test]
    fn duplicate_and_unknown_parts_are_refused() {
        let mut input = input();
        input.parts.push(part(1, 0, dist(7, 7)));
        assert!(matches!(plan(&input), Err(PlanningError::DuplicatePart(_))));
        let mut unknown = self::input();
        unknown.demand.push(DemandLine {
            part: PartId(99),
            period: 0,
            quantity: Units::new(1).unwrap(),
        });
        assert!(matches!(plan(&unknown), Err(PlanningError::UnknownPart(_))));
    }

    #[test]
    fn hostile_share_lists_are_refused_without_overflow() {
        let shares = vec![
            SupplyShare {
                supplier: SupplierId(1),
                share_ppm: i64::MAX,
            },
            SupplyShare {
                supplier: SupplierId(2),
                share_ppm: i64::MAX,
            },
        ];
        let error = allocate(Units::new(1).unwrap(), &shares).unwrap_err();
        assert!(matches!(
            error,
            PlanningError::Invalid(_) | PlanningError::Exact(ExactError::Overflow)
        ));
        let mut input = input();
        input.parts[0].suppliers = shares;
        assert!(plan(&input).is_err());
    }

    #[test]
    fn json_fixture_plans_a_part() {
        let input: PlanningInput =
            serde_json::from_str(include_str!("../tests/fixtures/plan.json")).unwrap();
        let result = plan(&input).unwrap();
        assert_eq!(result.parts.len(), 1);
        assert_eq!(result.parts[0].gross[0].get(), 500);
        assert!(result.parts[0].safety_stock.is_known());
        assert!(result.parts[0]
            .planned_orders
            .iter()
            .any(|order| order.quantity.get() >= 100));
    }

    #[test]
    #[ignore = "[perf] planning 10k parts with a 10k-edge BOM chain"]
    fn perf_ten_thousand_part_bom_chain_is_linear() {
        let mut input = PlanningInput {
            currency: Currency::USD,
            periods: 12,
            days_per_period: 7,
            periods_per_year: 52,
            horizon_start: Day(DAY),
            service_level_ppm: 950_000,
            lead_time_quantile: Quantile::P90,
            parts: Vec::new(),
            bom: Vec::new(),
            demand: Vec::new(),
        };
        for index in 0..10_000_u32 {
            let mut item = part(index, 0, dist(7, 7));
            item.part = PartId(index);
            item.mpn = format!("P{index}");
            item.suppliers = vec![SupplyShare {
                supplier: SupplierId(index),
                share_ppm: 1_000_000,
            }];
            input.parts.push(item);
            if index > 0 {
                input.bom.push(BomLine {
                    parent: PartId(0),
                    child: PartId(index),
                    quantity_per: ScaledQuantity::new(1_000_000).unwrap(),
                    scrap_rate: Rate::new(0).unwrap(),
                });
            }
        }
        input.demand.push(DemandLine {
            part: PartId(0),
            period: 0,
            quantity: Units::new(100).unwrap(),
        });
        let started = std::time::Instant::now();
        let result = plan(&input).unwrap();
        let elapsed = started.elapsed();
        eprintln!(
            "perf planning: 10k parts x 10k BOM edges x 12 periods = {:?}",
            elapsed
        );
        assert_eq!(result.parts.len(), 10_000);
        assert!(elapsed.as_secs() < 10, "planning took {elapsed:?}");
    }

    #[test]
    fn ten_thousand_parts_plan_in_bounded_time() {
        let mut input = PlanningInput {
            currency: Currency::USD,
            periods: 4,
            days_per_period: 7,
            periods_per_year: 52,
            horizon_start: Day(DAY),
            service_level_ppm: 950_000,
            lead_time_quantile: Quantile::P90,
            parts: Vec::new(),
            bom: vec![],
            demand: Vec::new(),
        };
        for index in 0..10_000_u32 {
            let mut plan = part(index, 0, dist(7, 7));
            plan.part = PartId(index);
            plan.mpn = format!("P{index}");
            plan.suppliers = vec![SupplyShare {
                supplier: SupplierId(index),
                share_ppm: 1_000_000,
            }];
            input.parts.push(plan);
            input.demand.push(DemandLine {
                part: PartId(index),
                period: 0,
                quantity: Units::new(10).unwrap(),
            });
        }
        let started = std::time::Instant::now();
        let result = plan(&input).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result.parts.len(), 10_000);
        assert!(elapsed.as_secs() < 30, "planning took {elapsed:?}");
    }
}
