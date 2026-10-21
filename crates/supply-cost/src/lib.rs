//! faktor-supply-cost — exact should-cost and risk-adjusted landed TCO.
//!
//! # Should-cost model (manufactured item)
//!
//! ```text
//! material        = sum(quantity_per_unit / material_yield * unit_price)
//! machine         = rate_per_hour * cycle_seconds / 3600
//! setup           = setup_cost / lot_size
//! tooling         = tooling_cost / amortization_units
//! labor           = rate_per_hour * minutes_per_unit / 60
//! route step cost = (machine + setup + tooling + labor) / cumulative_route_yield
//! inspection      = explicit per-unit cost
//! scrap           = (material + route + inspection) * scrap_rate
//! rework          = rework_rate * rework_cost_per_unit
//! overhead        = selected base * overhead_rate
//! packaging       = explicit per-unit cost
//! margin          = markup on cost OR margin on price
//! ex-works        = material + route + inspection + scrap + rework + overhead
//!                   + packaging + margin
//! ```
//!
//! # Risk-adjusted TCO
//!
//! ```text
//! total = ex-works * quantity
//!       + origin inland + export handling + international freight + insurance
//!       + import handling + duty + tax + brokerage + final mile
//!       + working capital + expected quality + expected expedite
//!       + excess-MOQ carrying + obsolescence
//! ```
//!
//! Every term is a [`Term`]: known only with evidence or an explicit
//! assumption. An unavailable term makes the total unavailable and is listed
//! by name. It is never replaced by zero. An explicitly `not_applicable` term
//! contributes zero and is marked as such. All arithmetic is exact integer
//! micro-units with `i128` intermediates and documented rounding.

use faktor_supply_core::{
    mul_div_round_half_up, mul_div_round_up, validate_text, Assumption, Availability, Basis,
    Currency, ExactError, Money, Rate, ScaledQuantity, Term, Unavailable, UnavailableReason, Units,
    MAX_TEXT_BYTES,
};
use serde::{Deserialize, Serialize};

/// Maximum material lines in one should-cost.
pub const MAX_MATERIAL_LINES: usize = 64;
/// Maximum process steps.
pub const MAX_PROCESS_STEPS: usize = 64;
/// Maximum TCO lines (fixed vocabulary).
pub const MAX_TCO_TERMS: usize = 16;
/// Conservative rounding for yield inflation and requirements.
const PPM: i128 = 1_000_000;

/// A refused cost computation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CostError {
    /// An exact value or operation failed.
    #[error(transparent)]
    Exact(#[from] ExactError),
    /// The input has no material lines.
    #[error("should-cost requires at least one material line")]
    NoMaterials,
    /// The input has no process steps.
    #[error("should-cost requires at least one process step")]
    NoProcessSteps,
    /// A quantity needed for amortization is zero.
    #[error("zero {0}")]
    ZeroQuantity(&'static str),
    /// A term is in a different currency than the analysis.
    #[error("term currency {actual} does not match analysis currency {expected}")]
    CurrencyMismatch {
        /// Analysis currency.
        expected: Currency,
        /// Term currency.
        actual: Currency,
    },
    /// A margin of 100% or more on price is undefined.
    #[error("margin on price must be below 100%")]
    MarginUndefined,
    /// A negative amount where a cost is expected.
    #[error("{0} must not be negative")]
    NegativeAmount(&'static str),
}

fn exact(value: Result<Money, ExactError>) -> Result<Money, CostError> {
    value.map_err(CostError::Exact)
}

// --------------------------------------------------------------- should-cost

/// One material input per finished unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialLine {
    /// Material description.
    pub description: String,
    /// Quantity consumed per finished unit, in micro base units.
    pub quantity_per_unit: ScaledQuantity,
    /// Cost per whole base unit.
    pub unit_price: Money,
    /// Usable yield of the material (input requirement = qty / yield).
    pub yield_rate: Rate,
}

/// Machine time for one process step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineTime {
    /// Machine / work-center rate per hour.
    pub rate_per_hour: Money,
    /// Cycle time in seconds.
    pub cycle_seconds: u32,
}

/// Setup amortization for one process step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupCost {
    /// Setup cost per lot.
    pub setup_cost: Money,
    /// Lot size over which the setup is amortized.
    pub lot_size: Units,
}

/// Tooling / NRE amortization for one process step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolingAmortization {
    /// One-time tooling or NRE cost.
    pub tooling_cost: Money,
    /// Units over which it is amortized.
    pub amortization_units: Units,
}

/// Direct labor for one process step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaborTime {
    /// Fully burdened labor rate per hour.
    pub rate_per_hour: Money,
    /// Direct minutes per unit (micro-minutes would be overkill; whole
    /// minutes keep the input auditable).
    pub minutes_per_unit: u32,
}

/// One ordered process-route step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessStep {
    /// Operation name.
    pub operation: String,
    /// Machine time and rate.
    pub machine: Term<MachineTime>,
    /// Setup amortization.
    pub setup: Term<SetupCost>,
    /// Tooling amortization.
    pub tooling: Term<ToolingAmortization>,
    /// Direct labor.
    pub labor: Term<LaborTime>,
    /// Step yield (good output / input).
    pub yield_rate: Term<Rate>,
}

/// Scrap and rework model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrapRework {
    /// Scrap rate applied to material + route + inspection.
    pub scrap_rate: Rate,
    /// Rework rate applied to the rework cost.
    pub rework_rate: Rate,
    /// Cost of one rework.
    pub rework_cost_per_unit: Money,
}

/// The overhead allocation base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverheadBase {
    /// Direct labor only.
    DirectLabor,
    /// Machine + setup + tooling + labor.
    Conversion,
    /// Material + conversion.
    MaterialAndConversion,
    /// Everything before overhead (including inspection, scrap, rework).
    TotalBeforeOverhead,
}

/// Overhead model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overhead {
    /// Rate applied to the selected base.
    pub rate: Rate,
    /// The base.
    pub base: OverheadBase,
}

/// How margin is expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarginMode {
    /// Price = cost * (1 + margin).
    MarkupOnCost,
    /// Price = cost / (1 - margin); margin must be below 100%.
    MarginOnPrice,
}

/// Complete should-cost input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShouldCostInput {
    /// Analysis currency. Every known money term must match it.
    pub currency: Currency,
    /// Material lines.
    pub materials: Vec<Term<MaterialLine>>,
    /// Ordered process route.
    pub process_steps: Vec<ProcessStep>,
    /// Inspection / test cost per unit.
    pub inspection: Term<Money>,
    /// Scrap and rework model.
    pub scrap_rework: Term<ScrapRework>,
    /// Overhead model.
    pub overhead: Term<Overhead>,
    /// Packaging cost per unit.
    pub packaging: Term<Money>,
    /// Margin.
    pub margin: Term<Rate>,
    /// How margin is expressed.
    pub margin_mode: MarginMode,
}

impl ShouldCostInput {
    /// Validate bounds, currencies and explicit evidence.
    pub fn validate(&self) -> Result<(), CostError> {
        if self.materials.is_empty() {
            return Err(CostError::NoMaterials);
        }
        if self.process_steps.is_empty() {
            return Err(CostError::NoProcessSteps);
        }
        if self.materials.len() > MAX_MATERIAL_LINES {
            return Err(CostError::Exact(ExactError::OutOfBounds {
                what: "materials",
                detail: format!("{} entries", self.materials.len()),
            }));
        }
        if self.process_steps.len() > MAX_PROCESS_STEPS {
            return Err(CostError::Exact(ExactError::OutOfBounds {
                what: "process steps",
                detail: format!("{} entries", self.process_steps.len()),
            }));
        }
        for term in &self.materials {
            term.validate()?;
            if let Some(material) = term.as_known() {
                validate_text(
                    &material.description,
                    MAX_TEXT_BYTES,
                    "material description",
                )?;
                check_currency(material.unit_price, self.currency)?;
                check_non_negative(material.unit_price, "material price")?;
                material
                    .quantity_per_unit
                    .non_negative("material quantity")?;
            }
        }
        for step in &self.process_steps {
            validate_text(&step.operation, MAX_TEXT_BYTES, "operation")?;
            step.machine.validate()?;
            step.setup.validate()?;
            step.tooling.validate()?;
            step.labor.validate()?;
            step.yield_rate.validate()?;
            if let Some(machine) = step.machine.as_known() {
                check_currency(machine.rate_per_hour, self.currency)?;
                check_non_negative(machine.rate_per_hour, "machine rate")?;
            }
            if let Some(setup) = step.setup.as_known() {
                check_currency(setup.setup_cost, self.currency)?;
                check_non_negative(setup.setup_cost, "setup cost")?;
                if setup.lot_size.is_zero() {
                    return Err(CostError::ZeroQuantity("lot size"));
                }
            }
            if let Some(tooling) = step.tooling.as_known() {
                check_currency(tooling.tooling_cost, self.currency)?;
                check_non_negative(tooling.tooling_cost, "tooling cost")?;
                if tooling.amortization_units.is_zero() {
                    return Err(CostError::ZeroQuantity("tooling amortization units"));
                }
            }
            if let Some(labor) = step.labor.as_known() {
                check_currency(labor.rate_per_hour, self.currency)?;
                check_non_negative(labor.rate_per_hour, "labor rate")?;
            }
            if let Some(yield_rate) = step.yield_rate.as_known() {
                if yield_rate.ppm() == 0 {
                    return Err(CostError::ZeroQuantity("process yield"));
                }
            }
        }
        for term in [&self.inspection, &self.packaging] {
            term.validate()?;
            if let Some(value) = term.as_known() {
                check_currency(*value, self.currency)?;
                check_non_negative(*value, "cost term")?;
            }
        }
        self.scrap_rework.validate()?;
        if let Some(scrap) = self.scrap_rework.as_known() {
            check_currency(scrap.rework_cost_per_unit, self.currency)?;
            check_non_negative(scrap.rework_cost_per_unit, "rework cost")?;
        }
        self.overhead.validate()?;
        self.margin.validate()?;
        if let (Some(margin), MarginMode::MarginOnPrice) =
            (self.margin.as_known(), self.margin_mode)
        {
            if margin.ppm() >= 1_000_000 {
                return Err(CostError::MarginUndefined);
            }
        }
        Ok(())
    }
}

fn check_currency(money: Money, expected: Currency) -> Result<(), CostError> {
    if money.currency != expected {
        return Err(CostError::CurrencyMismatch {
            expected,
            actual: money.currency,
        });
    }
    Ok(())
}

fn check_non_negative(money: Money, what: &'static str) -> Result<(), CostError> {
    if money.micros() < 0 {
        return Err(CostError::NegativeAmount(what));
    }
    Ok(())
}

/// The named term in a should-cost breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostTerm {
    /// Raw material / purchased content.
    Material,
    /// Machine time.
    Machine,
    /// Setup amortization.
    Setup,
    /// Tooling / NRE amortization.
    Tooling,
    /// Direct labor.
    Labor,
    /// Inspection / test.
    Inspection,
    /// Scrap.
    Scrap,
    /// Rework.
    Rework,
    /// Overhead.
    Overhead,
    /// Packaging.
    Packaging,
    /// Margin.
    Margin,
}

/// One should-cost line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostLine {
    /// The term.
    pub term: CostTerm,
    /// Exact amount, or the typed reason it cannot be computed.
    pub amount: Availability<Money>,
    /// True when the term was explicitly declared not applicable (zero).
    pub not_applicable: bool,
    /// Evidence or assumptions behind every input in this line.
    #[serde(default)]
    pub basis: Vec<Basis>,
    /// Auditable explanation of the arithmetic.
    pub detail: String,
}

/// Complete should-cost result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShouldCost {
    /// Analysis currency.
    pub currency: Currency,
    /// Lines in fixed term order.
    pub lines: Vec<CostLine>,
    /// Ex-works cost per unit, or typed unavailable.
    pub ex_works: Availability<Money>,
    /// Terms that made the total unavailable (never silently zeroed).
    #[serde(default)]
    pub unavailable_terms: Vec<CostTerm>,
}

impl ShouldCost {
    /// The line for a term.
    pub fn line(&self, term: CostTerm) -> Option<&CostLine> {
        self.lines.iter().find(|line| line.term == term)
    }
}

/// Compute should-cost per unit. Deterministic and exact.
pub fn should_cost(input: &ShouldCostInput) -> Result<ShouldCost, CostError> {
    input.validate()?;
    let currency = input.currency;

    // Material: quantity / yield * price, rounded half up per line.
    let mut material = Money::zero(currency);
    let mut material_basis = Vec::new();
    let mut material_unknown = None;
    let mut material_details = Vec::new();
    for term in &input.materials {
        collect_basis(term, &mut material_basis);
        match term.value.clone() {
            Availability::Known { value } => match material_line_cost(&value) {
                Ok(cost) => match material.checked_add(cost) {
                    Ok(total) => {
                        material = total;
                        material_details.push(format!("{}: {}", value.description, cost));
                    }
                    Err(error) => {
                        material_unknown.get_or_insert(Unavailable::new(
                            UnavailableReason::Arithmetic,
                            error.to_string(),
                        ));
                    }
                },
                Err(unavailable) => {
                    material_unknown.get_or_insert(unavailable);
                }
            },
            Availability::Unavailable { unavailable } => {
                material_unknown.get_or_insert(unavailable);
            }
        }
    }
    let material_line = if let Some(unavailable) = material_unknown {
        unavailable_line(CostTerm::Material, material_basis.clone(), unavailable)
    } else {
        known_line(
            CostTerm::Material,
            material,
            material_basis.clone(),
            format!("sum(quantity/yield*price): {}", material_details.join("; ")),
        )
    };

    // Route: per-step machine, setup, tooling, labor inflated by cumulative
    // route yield. A step's cumulative yield includes its own yield.
    let mut machine = Money::zero(currency);
    let mut setup = Money::zero(currency);
    let mut tooling = Money::zero(currency);
    let mut labor = Money::zero(currency);
    let mut machine_basis = Vec::new();
    let mut setup_basis = Vec::new();
    let mut tooling_basis = Vec::new();
    let mut labor_basis = Vec::new();
    let mut route_unknown: Option<Unavailable> = None;
    let mut cumulative_yield = 1_000_000_i64;
    let mut route_details = Vec::new();
    for step in &input.process_steps {
        collect_basis(&step.machine, &mut machine_basis);
        collect_basis(&step.setup, &mut setup_basis);
        collect_basis(&step.tooling, &mut tooling_basis);
        collect_basis(&step.labor, &mut labor_basis);
        match step.yield_rate.value.clone() {
            Availability::Known { value } => {
                cumulative_yield = mul_div_round_half_up(
                    i128::from(cumulative_yield),
                    i128::from(value.ppm()),
                    PPM,
                )
                .and_then(|value| i64::try_from(value).ok())
                .unwrap_or(0);
                if cumulative_yield <= 0 {
                    route_unknown.get_or_insert(Unavailable::new(
                        UnavailableReason::Arithmetic,
                        format!("route yield became zero at {}", step.operation),
                    ));
                }
            }
            Availability::Unavailable { unavailable } => {
                route_unknown.get_or_insert(unavailable);
            }
        }
        let step_machine = step.machine.as_known().map(machine_step_cost);
        let step_setup = step.setup.as_known().map(setup_step_cost);
        let step_tooling = step.tooling.as_known().map(tooling_step_cost);
        let step_labor = step.labor.as_known().map(labor_step_cost);
        match (step_machine, step_setup, step_tooling, step_labor) {
            (
                Some(Ok(machine_cost)),
                Some(Ok(setup_cost)),
                Some(Ok(tooling_cost)),
                Some(Ok(labor_cost)),
            ) => {
                if cumulative_yield > 0 {
                    let yield_rate = Rate::new(cumulative_yield)?;
                    let inflated = (
                        inflate(machine_cost, yield_rate),
                        inflate(setup_cost, yield_rate),
                        inflate(tooling_cost, yield_rate),
                        inflate(labor_cost, yield_rate),
                    );
                    match inflated {
                        (Ok(m), Ok(s), Ok(t), Ok(l)) => {
                            machine = exact(machine.checked_add(m))?;
                            setup = exact(setup.checked_add(s))?;
                            tooling = exact(tooling.checked_add(t))?;
                            labor = exact(labor.checked_add(l))?;
                            route_details.push(format!(
                                "{}: machine {} setup {} tooling {} labor {} at cumulative yield {} ppm",
                                step.operation, m, s, t, l, cumulative_yield
                            ));
                        }
                        (m, s, t, l) => {
                            let error = [m.err(), s.err(), t.err(), l.err()]
                                .into_iter()
                                .flatten()
                                .next()
                                .unwrap_or(ExactError::Overflow);
                            route_unknown.get_or_insert(Unavailable::new(
                                UnavailableReason::Arithmetic,
                                error.to_string(),
                            ));
                        }
                    }
                }
            }
            (machine, setup, tooling, labor) => {
                let exact_error = [machine, setup, tooling, labor]
                    .into_iter()
                    .flatten()
                    .find_map(|result| result.err());
                route_unknown.get_or_insert(match exact_error {
                    Some(error) => {
                        Unavailable::new(UnavailableReason::Arithmetic, error.to_string())
                    }
                    None => Unavailable::new(
                        UnavailableReason::MissingEvidence,
                        format!("{} has an unavailable cost input", step.operation),
                    ),
                });
            }
        }
    }

    let machine_line = route_component_line(
        CostTerm::Machine,
        machine,
        machine_basis.clone(),
        &route_unknown,
        &route_details,
    );
    let setup_line = route_component_line(
        CostTerm::Setup,
        setup,
        setup_basis.clone(),
        &route_unknown,
        &route_details,
    );
    let tooling_line = route_component_line(
        CostTerm::Tooling,
        tooling,
        tooling_basis.clone(),
        &route_unknown,
        &route_details,
    );
    let labor_line = route_component_line(
        CostTerm::Labor,
        labor,
        labor_basis.clone(),
        &route_unknown,
        &route_details,
    );

    // Inspection.
    let inspection_line = simple_money_line(
        CostTerm::Inspection,
        &input.inspection,
        "inspection/test",
        currency,
    );

    let route_total = sum_known(&[&machine_line, &setup_line, &tooling_line, &labor_line])?;
    let material_total = sum_known(&[&material_line])?;

    // Scrap and rework base: material + route + inspection.
    let scrap_base = match (&material_total, &route_total, &inspection_line.amount) {
        (
            Availability::Known {
                value: material_value,
            },
            Availability::Known { value: route_value },
            Availability::Known {
                value: inspection_value,
            },
        ) => material_value
            .checked_add(*route_value)
            .and_then(|value| value.checked_add(*inspection_value))
            .map(Availability::known)
            .map_err(|error| {
                CostError::Exact(ExactError::OutOfBounds {
                    what: "scrap base",
                    detail: error.to_string(),
                })
            })?,
        _ => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "scrap base needs material, route and inspection",
        ),
    };
    let mut scrap_basis = material_basis.clone();
    scrap_basis.extend(machine_basis.iter().cloned());
    scrap_basis.extend(setup_basis.iter().cloned());
    scrap_basis.extend(tooling_basis.iter().cloned());
    scrap_basis.extend(labor_basis.iter().cloned());
    scrap_basis.extend(inspection_line.basis.clone());
    collect_basis(&input.scrap_rework, &mut scrap_basis);
    let (scrap_line, rework_line) = match input.scrap_rework.value.clone() {
        Availability::Unavailable { unavailable } if unavailable.is_not_applicable() => (
            not_applicable_line(CostTerm::Scrap, "scrap not applicable", currency),
            not_applicable_line(CostTerm::Rework, "rework not applicable", currency),
        ),
        Availability::Unavailable { unavailable } => (
            unavailable_line(CostTerm::Scrap, scrap_basis.clone(), unavailable.clone()),
            unavailable_line(CostTerm::Rework, scrap_basis, unavailable.clone()),
        ),
        Availability::Known { value } => match &scrap_base {
            Availability::Known { value: base } => {
                let scrap_amount = base.scale(value.scrap_rate);
                let rework_amount = value.rework_cost_per_unit.scale(value.rework_rate);
                match (scrap_amount, rework_amount) {
                    (Ok(scrap), Ok(rework)) => (
                        known_line(
                            CostTerm::Scrap,
                            scrap,
                            scrap_basis.clone(),
                            format!("{} x scrap rate {}", base, value.scrap_rate.as_fraction()),
                        ),
                        known_line(
                            CostTerm::Rework,
                            rework,
                            scrap_basis,
                            format!(
                                "{} x rework rate {}",
                                value.rework_cost_per_unit,
                                value.rework_rate.as_fraction()
                            ),
                        ),
                    ),
                    (scrap, rework) => {
                        let error = scrap
                            .err()
                            .or_else(|| rework.err())
                            .unwrap_or(ExactError::Overflow);
                        (
                            unavailable_line(
                                CostTerm::Scrap,
                                scrap_basis.clone(),
                                Unavailable::new(UnavailableReason::Arithmetic, error.to_string()),
                            ),
                            unavailable_line(
                                CostTerm::Rework,
                                scrap_basis,
                                Unavailable::new(UnavailableReason::Arithmetic, error.to_string()),
                            ),
                        )
                    }
                }
            }
            Availability::Unavailable { unavailable } => (
                unavailable_line(CostTerm::Scrap, scrap_basis.clone(), unavailable.clone()),
                unavailable_line(CostTerm::Rework, scrap_basis, unavailable.clone()),
            ),
        },
    };

    let labor_total = sum_known(&[&labor_line])?;
    let mut overhead_basis = machine_basis.clone();
    overhead_basis.extend(setup_basis.iter().cloned());
    overhead_basis.extend(tooling_basis.iter().cloned());
    overhead_basis.extend(labor_basis.iter().cloned());
    overhead_basis.extend(material_basis.iter().cloned());
    overhead_basis.extend(inspection_line.basis.clone());
    overhead_basis.extend(scrap_line.basis.clone());
    overhead_basis.extend(rework_line.basis.clone());
    collect_basis(&input.overhead, &mut overhead_basis);
    let overhead_line = match input.overhead.value.clone() {
        Availability::Unavailable { unavailable } if unavailable.is_not_applicable() => {
            not_applicable_line(CostTerm::Overhead, "overhead not applicable", currency)
        }
        Availability::Unavailable { unavailable } => {
            unavailable_line(CostTerm::Overhead, overhead_basis, unavailable)
        }
        Availability::Known { value } => {
            let base = match value.base {
                OverheadBase::DirectLabor => labor_total.clone(),
                OverheadBase::Conversion => route_total.clone(),
                OverheadBase::MaterialAndConversion => match (&material_total, &route_total) {
                    (
                        Availability::Known {
                            value: material_value,
                        },
                        Availability::Known { value: conversion },
                    ) => match material_value.checked_add(*conversion) {
                        Ok(value) => Availability::Known { value },
                        Err(error) => Availability::unavailable(
                            UnavailableReason::Arithmetic,
                            error.to_string(),
                        ),
                    },
                    _ => Availability::unavailable(
                        UnavailableReason::MissingEvidence,
                        "overhead base needs material and conversion",
                    ),
                },
                OverheadBase::TotalBeforeOverhead => {
                    match sum_known(&[
                        &material_line,
                        &machine_line,
                        &setup_line,
                        &tooling_line,
                        &labor_line,
                        &inspection_line,
                        &scrap_line,
                        &rework_line,
                    ])? {
                        Availability::Known { value } => Availability::Known { value },
                        _ => Availability::unavailable(
                            UnavailableReason::MissingEvidence,
                            "overhead base needs all pre-overhead lines",
                        ),
                    }
                }
            };
            match base {
                Availability::Known { value: base_value } => match base_value.scale(value.rate) {
                    Ok(amount) => known_line(
                        CostTerm::Overhead,
                        amount,
                        overhead_basis,
                        format!("{} x overhead {}", base_value, value.rate.as_fraction()),
                    ),
                    Err(error) => unavailable_line(
                        CostTerm::Overhead,
                        overhead_basis,
                        Unavailable::new(UnavailableReason::Arithmetic, error.to_string()),
                    ),
                },
                Availability::Unavailable { unavailable } => {
                    unavailable_line(CostTerm::Overhead, overhead_basis, unavailable)
                }
            }
        }
    };

    let packaging_line =
        simple_money_line(CostTerm::Packaging, &input.packaging, "packaging", currency);

    let pre_margin = sum_known(&[
        &material_line,
        &machine_line,
        &setup_line,
        &tooling_line,
        &labor_line,
        &inspection_line,
        &scrap_line,
        &rework_line,
        &overhead_line,
        &packaging_line,
    ])?;
    let mut margin_basis = material_basis;
    margin_basis.extend(machine_basis);
    margin_basis.extend(setup_basis);
    margin_basis.extend(tooling_basis);
    margin_basis.extend(labor_basis);
    margin_basis.extend(inspection_line.basis.clone());
    margin_basis.extend(scrap_line.basis.clone());
    margin_basis.extend(rework_line.basis.clone());
    margin_basis.extend(overhead_line.basis.clone());
    margin_basis.extend(packaging_line.basis.clone());
    collect_basis(&input.margin, &mut margin_basis);

    let (margin_line, ex_works) = match (&pre_margin, input.margin.value.clone()) {
        (Availability::Unavailable { unavailable }, _) => (
            unavailable_line(CostTerm::Margin, margin_basis, unavailable.clone()),
            Availability::Unavailable {
                unavailable: unavailable.clone(),
            },
        ),
        (_, Availability::Unavailable { unavailable }) if unavailable.is_not_applicable() => (
            not_applicable_line(CostTerm::Margin, "margin not applicable", currency),
            pre_margin.clone(),
        ),
        (_, Availability::Unavailable { unavailable }) => (
            unavailable_line(CostTerm::Margin, margin_basis, unavailable.clone()),
            Availability::Unavailable { unavailable },
        ),
        (Availability::Known { value: cost }, Availability::Known { value: margin_rate }) => {
            let price = match input.margin_mode {
                MarginMode::MarkupOnCost => {
                    let micros = mul_div_round_half_up(
                        i128::from(cost.micros()),
                        1_000_000 + i128::from(margin_rate.ppm()),
                        1_000_000,
                    )
                    .ok_or(CostError::Exact(ExactError::Overflow))?;
                    let micros = i64::try_from(micros)
                        .map_err(|_| CostError::Exact(ExactError::Overflow))?;
                    Money::from_micros(micros, currency).map_err(CostError::Exact)?
                }
                MarginMode::MarginOnPrice => {
                    if margin_rate.ppm() >= 1_000_000 {
                        return Err(CostError::MarginUndefined);
                    }
                    let micros = mul_div_round_half_up(
                        i128::from(cost.micros()),
                        PPM,
                        i128::from(margin_rate.complement().ppm()),
                    )
                    .ok_or(CostError::Exact(ExactError::Overflow))?;
                    let micros = i64::try_from(micros)
                        .map_err(|_| CostError::Exact(ExactError::Overflow))?;
                    Money::from_micros(micros, currency).map_err(CostError::Exact)?
                }
            };
            let margin_amount = price.checked_sub(*cost)?;
            (
                known_line(
                    CostTerm::Margin,
                    margin_amount,
                    margin_basis,
                    format!(
                        "{} margin {:?} on cost {} -> price {}",
                        margin_rate.as_fraction(),
                        input.margin_mode,
                        cost,
                        price
                    ),
                ),
                Availability::Known { value: price },
            )
        }
    };

    let lines = vec![
        material_line,
        machine_line,
        setup_line,
        tooling_line,
        labor_line,
        inspection_line,
        scrap_line,
        rework_line,
        overhead_line,
        packaging_line,
        margin_line,
    ];
    let unavailable_terms = lines
        .iter()
        .filter(|line| !line.amount.is_known())
        .map(|line| line.term)
        .collect::<Vec<_>>();
    let ex_works = if unavailable_terms.is_empty() {
        ex_works
    } else {
        Availability::Unavailable {
            unavailable: Unavailable::new(
                UnavailableReason::MissingEvidence,
                format!(
                    "unavailable terms: {}",
                    unavailable_terms
                        .iter()
                        .map(|term| format!("{term:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        }
    };
    Ok(ShouldCost {
        currency,
        lines,
        ex_works,
        unavailable_terms,
    })
}

fn material_line_cost(material: &MaterialLine) -> Result<Money, Unavailable> {
    material
        .quantity_per_unit
        .divide_by_yield(material.yield_rate)
        .map_err(|error| Unavailable::new(UnavailableReason::Arithmetic, error.to_string()))
        .and_then(|quantity| {
            quantity
                .cost(material.unit_price)
                .map_err(|error| Unavailable::new(UnavailableReason::Arithmetic, error.to_string()))
        })
}

fn machine_step_cost(input: &MachineTime) -> Result<Money, ExactError> {
    let micros = mul_div_round_half_up(
        i128::from(input.rate_per_hour.micros()),
        i128::from(input.cycle_seconds),
        3_600,
    )
    .ok_or(ExactError::Overflow)?;
    Money::from_micros(
        i64::try_from(micros).map_err(|_| ExactError::Overflow)?,
        input.rate_per_hour.currency,
    )
}

fn setup_step_cost(input: &SetupCost) -> Result<Money, ExactError> {
    input.setup_cost.div_units(input.lot_size.get())
}

fn tooling_step_cost(input: &ToolingAmortization) -> Result<Money, ExactError> {
    input.tooling_cost.div_units(input.amortization_units.get())
}

fn labor_step_cost(input: &LaborTime) -> Result<Money, ExactError> {
    let micros = mul_div_round_half_up(
        i128::from(input.rate_per_hour.micros()),
        i128::from(input.minutes_per_unit),
        60,
    )
    .ok_or(ExactError::Overflow)?;
    Money::from_micros(
        i64::try_from(micros).map_err(|_| ExactError::Overflow)?,
        input.rate_per_hour.currency,
    )
}

fn inflate(money: Money, cumulative_yield: Rate) -> Result<Money, ExactError> {
    let micros = mul_div_round_up(
        i128::from(money.micros()),
        PPM,
        i128::from(cumulative_yield.ppm()),
    )
    .ok_or(ExactError::Overflow)?;
    Money::from_micros(
        i64::try_from(micros).map_err(|_| ExactError::Overflow)?,
        money.currency,
    )
}

fn route_component_line(
    term: CostTerm,
    amount: Money,
    basis: Vec<Basis>,
    route_unknown: &Option<Unavailable>,
    details: &[String],
) -> CostLine {
    if let Some(unavailable) = route_unknown {
        unavailable_line(term, basis, unavailable.clone())
    } else {
        known_line(term, amount, basis, details.join("; "))
    }
}

fn simple_money_line(
    term: CostTerm,
    input: &Term<Money>,
    label: &str,
    currency: Currency,
) -> CostLine {
    let mut basis = Vec::new();
    collect_basis(input, &mut basis);
    match input.value.clone() {
        Availability::Known { value } => known_line(term, value, basis, label.to_string()),
        Availability::Unavailable { unavailable } if unavailable.is_not_applicable() => {
            not_applicable_line(term, format!("{label} not applicable"), currency)
        }
        Availability::Unavailable { unavailable } => unavailable_line(term, basis, unavailable),
    }
}

fn known_line(term: CostTerm, amount: Money, basis: Vec<Basis>, detail: String) -> CostLine {
    CostLine {
        term,
        amount: Availability::known(amount),
        not_applicable: false,
        basis,
        detail,
    }
}

fn unavailable_line(term: CostTerm, basis: Vec<Basis>, unavailable: Unavailable) -> CostLine {
    CostLine {
        term,
        amount: Availability::Unavailable { unavailable },
        not_applicable: false,
        basis,
        detail: format!("{term:?} unavailable"),
    }
}

fn not_applicable_line(term: CostTerm, detail: impl Into<String>, currency: Currency) -> CostLine {
    CostLine {
        term,
        amount: Availability::known(Money::zero(currency)),
        not_applicable: true,
        basis: Vec::new(),
        detail: detail.into(),
    }
}

fn sum_known(lines: &[&CostLine]) -> Result<Availability<Money>, CostError> {
    let mut total: Option<Money> = None;
    for line in lines {
        match &line.amount {
            Availability::Known { value } => {
                total = Some(match total {
                    Some(current) => exact(current.checked_add(*value))?,
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
        None => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "no cost lines".to_string(),
        ),
    })
}

fn collect_basis<T>(term: &Term<T>, basis: &mut Vec<Basis>) {
    if let Some(value) = term.basis() {
        let duplicate = basis.iter().any(|existing| same_basis(existing, value));
        if !duplicate {
            basis.push(value.clone());
        }
    }
}

/// Identity comparison without allocating a key string per candidate, so
/// accumulating a line's basis stays O(n^2) in comparisons only for the
/// (bounded) basis list rather than also allocating on every probe.
fn same_basis(left: &Basis, right: &Basis) -> bool {
    match (left, right) {
        (Basis::Provenance(left), Basis::Provenance(right)) => {
            left.evidence.id == right.evidence.id
                && left.evidence.revision == right.evidence.revision
        }
        (Basis::Assumption(left), Basis::Assumption(right)) => left.id == right.id,
        _ => false,
    }
}

/// A refused TCO term.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TcoError {
    /// Exact value failure.
    #[error(transparent)]
    Exact(#[from] ExactError),
    /// A shipment quantity of zero cannot be normalized per unit.
    #[error("TCO quantity must be positive")]
    ZeroQuantity,
    /// A known money term is in the wrong currency.
    #[error("term currency {actual} does not match analysis currency {expected}")]
    CurrencyMismatch {
        /// Analysis currency.
        expected: Currency,
        /// Term currency.
        actual: Currency,
    },
    /// A negative cost term.
    #[error("{0} must not be negative")]
    NegativeAmount(&'static str),
    /// A list exceeded its bound.
    #[error("{what} exceeds {max} entries")]
    Bound {
        /// The class.
        what: &'static str,
        /// The maximum.
        max: usize,
    },
}

/// Insurance premium.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Insurance {
    /// Insured value.
    pub insured_value: Money,
    /// Premium rate.
    pub rate: Rate,
    /// Minimum premium.
    pub minimum_premium: Money,
}

/// Duty computation inputs. The customs value is explicit so the duty does
/// not depend circularly on an insurance premium computed later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Duty {
    /// Customs value used by the tariff authority.
    pub customs_value: Money,
    /// Ad-valorem duty rate.
    pub rate: Rate,
    /// Fixed duty / fee.
    pub fixed: Money,
}

/// Tax computation inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tax {
    /// Taxable value.
    pub taxable_value: Money,
    /// Tax rate.
    pub rate: Rate,
    /// Recoverable taxes are disclosed but excluded from net landed cost.
    pub recoverable: bool,
}

/// Working-capital cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkingCapital {
    /// Annual capital cost rate.
    pub annual_rate: Rate,
    /// Days the capital is tied up.
    pub days: u32,
    /// Explicit financed amount (for example goods + freight).
    pub financed_amount: Money,
}

/// Expected quality cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityCost {
    /// Defect rate in defective parts per million.
    pub defect_ppm: i64,
    /// Units exposed.
    pub units: Units,
    /// Cost per defect.
    pub cost_per_defect: Money,
}

/// Expected expedite cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpediteCost {
    /// Probability of an expedite event.
    pub probability: Rate,
    /// Cost if it occurs.
    pub cost: Money,
}

/// Excess-MOQ inventory carrying cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExcessMoq {
    /// Units bought only because of the MOQ.
    pub excess_units: Units,
    /// Value per unit.
    pub unit_value: Money,
    /// Annual inventory carrying rate.
    pub annual_carrying_rate: Rate,
    /// Days carried.
    pub days: u32,
}

/// Obsolescence exposure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Obsolescence {
    /// Exposure rate.
    pub exposure_rate: Rate,
    /// Inventory value exposed.
    pub inventory_value: Money,
}

/// The TCO term vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TcoTerm {
    /// Ex-works goods value.
    ExWorks,
    /// Origin inland freight.
    OriginInlandFreight,
    /// Export handling.
    ExportHandling,
    /// International freight.
    InternationalFreight,
    /// Cargo insurance.
    Insurance,
    /// Import handling.
    ImportHandling,
    /// Duty.
    Duty,
    /// Tax.
    Tax,
    /// Brokerage.
    Brokerage,
    /// Final mile.
    FinalMile,
    /// Working-capital cost.
    WorkingCapital,
    /// Expected quality cost.
    ExpectedQuality,
    /// Expected expedite cost.
    ExpectedExpedite,
    /// Excess-MOQ carrying.
    ExcessMoqCarrying,
    /// Obsolescence exposure.
    Obsolescence,
}

/// One TCO line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcoLine {
    /// Term.
    pub term: TcoTerm,
    /// Amount or typed absence.
    pub amount: Availability<Money>,
    /// True when the term was explicitly declared not applicable (zero).
    pub not_applicable: bool,
    /// Evidence or assumptions.
    #[serde(default)]
    pub basis: Vec<Basis>,
    /// Explanation.
    pub detail: String,
}

/// TCO input. Every term is required to be present — either known with a
/// basis, unknown with a reason, or explicitly not applicable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TcoInput {
    /// Analysis currency.
    pub currency: Currency,
    /// Shipment quantity.
    pub quantity: Units,
    /// Ex-works unit price.
    pub ex_works_unit: Term<Money>,
    /// Origin inland freight (total).
    pub origin_inland_freight: Term<Money>,
    /// Export handling (total).
    pub export_handling: Term<Money>,
    /// International freight (total).
    pub international_freight: Term<Money>,
    /// Insurance.
    pub insurance: Term<Insurance>,
    /// Import handling (total).
    pub import_handling: Term<Money>,
    /// Duty.
    pub duty: Term<Duty>,
    /// Tax.
    pub tax: Term<Tax>,
    /// Brokerage (total).
    pub brokerage: Term<Money>,
    /// Final mile (total).
    pub final_mile: Term<Money>,
    /// Working capital.
    pub working_capital: Term<WorkingCapital>,
    /// Expected quality cost.
    pub expected_quality: Term<QualityCost>,
    /// Expected expedite cost.
    pub expected_expedite: Term<ExpediteCost>,
    /// Excess MOQ carrying.
    pub excess_moq_carrying: Term<ExcessMoq>,
    /// Obsolescence exposure.
    pub obsolescence: Term<Obsolescence>,
}

impl TcoInput {
    /// Validate currencies, bounds and evidence laws.
    pub fn validate(&self) -> Result<(), TcoError> {
        if self.quantity.is_zero() {
            return Err(TcoError::ZeroQuantity);
        }
        self.ex_works_unit.validate()?;
        if let Some(value) = self.ex_works_unit.as_known() {
            self.money(*value, "ex-works unit")?;
        }
        for term in [
            &self.origin_inland_freight,
            &self.export_handling,
            &self.international_freight,
            &self.import_handling,
            &self.brokerage,
            &self.final_mile,
        ] {
            term.validate()?;
            if let Some(value) = term.as_known() {
                self.money(*value, "freight/handling term")?;
            }
        }
        self.insurance.validate()?;
        if let Some(value) = self.insurance.as_known() {
            self.money(value.insured_value, "insured value")?;
            self.money(value.minimum_premium, "minimum premium")?;
        }
        self.duty.validate()?;
        if let Some(value) = self.duty.as_known() {
            self.money(value.customs_value, "customs value")?;
            self.money(value.fixed, "fixed duty")?;
        }
        self.tax.validate()?;
        if let Some(value) = self.tax.as_known() {
            self.money(value.taxable_value, "taxable value")?;
        }
        self.working_capital.validate()?;
        if let Some(value) = self.working_capital.as_known() {
            self.money(value.financed_amount, "financed amount")?;
        }
        self.expected_quality.validate()?;
        if let Some(value) = self.expected_quality.as_known() {
            self.money(value.cost_per_defect, "cost per defect")?;
            if value.defect_ppm < 0 {
                return Err(TcoError::NegativeAmount("defect ppm"));
            }
        }
        self.expected_expedite.validate()?;
        if let Some(value) = self.expected_expedite.as_known() {
            self.money(value.cost, "expedite cost")?;
        }
        self.excess_moq_carrying.validate()?;
        if let Some(value) = self.excess_moq_carrying.as_known() {
            self.money(value.unit_value, "excess MOQ unit value")?;
        }
        self.obsolescence.validate()?;
        if let Some(value) = self.obsolescence.as_known() {
            self.money(value.inventory_value, "obsolescence inventory value")?;
        }
        Ok(())
    }

    fn money(&self, value: Money, what: &'static str) -> Result<(), TcoError> {
        if value.currency != self.currency {
            return Err(TcoError::CurrencyMismatch {
                expected: self.currency,
                actual: value.currency,
            });
        }
        if value.micros() < 0 {
            return Err(TcoError::NegativeAmount(what));
        }
        Ok(())
    }
}

/// Complete landed-cost result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tco {
    /// Currency.
    pub currency: Currency,
    /// Quantity.
    pub quantity: Units,
    /// Lines in fixed order.
    pub lines: Vec<TcoLine>,
    /// Total landed cost, or typed unavailable.
    pub total: Availability<Money>,
    /// Per-unit landed cost, or typed unavailable.
    pub per_unit: Availability<Money>,
    /// Terms that made the total unavailable.
    #[serde(default)]
    pub unavailable_terms: Vec<TcoTerm>,
}

impl Tco {
    /// The line for a term.
    pub fn line(&self, term: TcoTerm) -> Option<&TcoLine> {
        self.lines.iter().find(|line| line.term == term)
    }
}

/// Compute risk-adjusted landed TCO. Every input term is resolved
/// independently; an unknown term is reported by name and makes the total
/// unavailable. Explicitly not-applicable terms contribute zero and are
/// flagged.
pub fn tco(input: &TcoInput) -> Result<Tco, TcoError> {
    input.validate()?;
    let currency = input.currency;

    let ex_works = resolve_money(
        TcoTerm::ExWorks,
        &input.ex_works_unit,
        input.currency,
        |unit| unit.mul_units(input.quantity.get()),
        "ex-works x quantity",
    );

    let origin = self::resolve_money(
        TcoTerm::OriginInlandFreight,
        &input.origin_inland_freight,
        currency,
        Ok,
        "origin inland freight",
    );
    let export = self::resolve_money(
        TcoTerm::ExportHandling,
        &input.export_handling,
        currency,
        Ok,
        "export handling",
    );
    let international = self::resolve_money(
        TcoTerm::InternationalFreight,
        &input.international_freight,
        currency,
        Ok,
        "international freight",
    );

    let insurance = resolve_struct(
        TcoTerm::Insurance,
        &input.insurance,
        input.currency,
        |value| {
            let premium = value.insured_value.scale(value.rate)?;
            if premium.micros() < value.minimum_premium.micros() {
                Ok(value.minimum_premium)
            } else {
                Ok(premium)
            }
        },
        "insured value x rate, floored at minimum premium",
    );

    let import = self::resolve_money(
        TcoTerm::ImportHandling,
        &input.import_handling,
        currency,
        Ok,
        "import handling",
    );
    let duty = resolve_struct(
        TcoTerm::Duty,
        &input.duty,
        currency,
        |value| {
            Ok(value
                .customs_value
                .scale(value.rate)?
                .checked_add(value.fixed)?)
        },
        "customs value x rate + fixed",
    );
    let tax = resolve_struct(
        TcoTerm::Tax,
        &input.tax,
        currency,
        |value| {
            if value.recoverable {
                Ok(Money::zero(currency))
            } else {
                Ok(value.taxable_value.scale(value.rate)?)
            }
        },
        "taxable value x rate (recoverable tax excluded from net cost)",
    );
    let brokerage = self::resolve_money(
        TcoTerm::Brokerage,
        &input.brokerage,
        currency,
        Ok,
        "brokerage",
    );
    let final_mile = self::resolve_money(
        TcoTerm::FinalMile,
        &input.final_mile,
        currency,
        Ok,
        "final mile",
    );
    let working_capital = resolve_struct(
        TcoTerm::WorkingCapital,
        &input.working_capital,
        currency,
        |value| {
            let annual = value.financed_amount.scale(value.annual_rate)?;
            let micros =
                mul_div_round_half_up(i128::from(annual.micros()), i128::from(value.days), 365)
                    .ok_or(TcoError::Exact(ExactError::Overflow))?;
            Money::from_micros(
                i64::try_from(micros).map_err(|_| TcoError::Exact(ExactError::Overflow))?,
                currency,
            )
            .map_err(TcoError::Exact)
        },
        "financed amount x annual rate x days / 365",
    );
    let quality = resolve_struct(
        TcoTerm::ExpectedQuality,
        &input.expected_quality,
        currency,
        |value| {
            // One exact product, one rounding: ppm * units * cost / 1e6.
            let numerator = i128::from(value.defect_ppm)
                .checked_mul(i128::from(value.units.get()))
                .and_then(|product| product.checked_mul(i128::from(value.cost_per_defect.micros())))
                .ok_or(TcoError::Exact(ExactError::Overflow))?;
            let cost_micros = mul_div_round_half_up(numerator, 1, PPM)
                .ok_or(TcoError::Exact(ExactError::Overflow))?;
            Money::from_micros(
                i64::try_from(cost_micros).map_err(|_| TcoError::Exact(ExactError::Overflow))?,
                currency,
            )
            .map_err(TcoError::Exact)
        },
        "defect ppm x units x cost per defect / 1e6",
    );
    let expedite = resolve_struct(
        TcoTerm::ExpectedExpedite,
        &input.expected_expedite,
        currency,
        |value| Ok(value.cost.scale(value.probability)?),
        "expedite cost x probability",
    );
    let excess = resolve_struct(
        TcoTerm::ExcessMoqCarrying,
        &input.excess_moq_carrying,
        currency,
        |value| {
            let value_total = value.unit_value.mul_units(value.excess_units.get())?;
            let annual = value_total.scale(value.annual_carrying_rate)?;
            let micros =
                mul_div_round_half_up(i128::from(annual.micros()), i128::from(value.days), 365)
                    .ok_or(TcoError::Exact(ExactError::Overflow))?;
            Money::from_micros(
                i64::try_from(micros).map_err(|_| TcoError::Exact(ExactError::Overflow))?,
                currency,
            )
            .map_err(TcoError::Exact)
        },
        "excess units x unit value x carrying rate x days / 365",
    );
    let obsolescence = resolve_struct(
        TcoTerm::Obsolescence,
        &input.obsolescence,
        currency,
        |value| Ok(value.inventory_value.scale(value.exposure_rate)?),
        "inventory value x exposure rate",
    );

    let lines = vec![
        ex_works,
        origin,
        export,
        international,
        insurance,
        import,
        duty,
        tax,
        brokerage,
        final_mile,
        working_capital,
        quality,
        expedite,
        excess,
        obsolescence,
    ];
    let unavailable_terms = lines
        .iter()
        .filter(|line| !line.amount.is_known())
        .map(|line| line.term)
        .collect::<Vec<_>>();
    let total = if unavailable_terms.is_empty() {
        match sum_tco(&lines)? {
            Availability::Known { value } => Availability::Known { value },
            Availability::Unavailable { unavailable } => Availability::Unavailable { unavailable },
        }
    } else {
        Availability::unavailable(
            UnavailableReason::MissingEvidence,
            format!(
                "unavailable terms: {}",
                unavailable_terms
                    .iter()
                    .map(|term| format!("{term:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    };
    let per_unit = match &total {
        Availability::Known { value } => match value.div_units(input.quantity.get()) {
            Ok(value) => Availability::Known { value },
            Err(error) => {
                Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
            }
        },
        Availability::Unavailable { unavailable } => Availability::Unavailable {
            unavailable: unavailable.clone(),
        },
    };
    Ok(Tco {
        currency,
        quantity: input.quantity,
        lines,
        total,
        per_unit,
        unavailable_terms,
    })
}

fn resolve_money<T: Clone>(
    term: TcoTerm,
    input: &Term<T>,
    currency: Currency,
    compute: impl FnOnce(T) -> Result<Money, ExactError>,
    detail: &str,
) -> TcoLine {
    let mut basis = Vec::new();
    collect_basis(input, &mut basis);
    match input.value.clone() {
        Availability::Known { value } => match compute(value) {
            Ok(amount) => TcoLine {
                term,
                amount: Availability::known(amount),
                not_applicable: false,
                basis,
                detail: detail.to_string(),
            },
            Err(error) => TcoLine {
                term,
                amount: Availability::unavailable(UnavailableReason::Arithmetic, error.to_string()),
                not_applicable: false,
                basis,
                detail: detail.to_string(),
            },
        },
        Availability::Unavailable { unavailable } if unavailable.is_not_applicable() => TcoLine {
            term,
            amount: Availability::known(Money::zero(currency)),
            not_applicable: true,
            basis,
            detail: format!("{detail} (not applicable)"),
        },
        Availability::Unavailable { unavailable } => TcoLine {
            term,
            amount: Availability::Unavailable { unavailable },
            not_applicable: false,
            basis,
            detail: detail.to_string(),
        },
    }
}

fn resolve_struct<T: Clone>(
    term: TcoTerm,
    input: &Term<T>,
    currency: Currency,
    compute: impl FnOnce(T) -> Result<Money, TcoError>,
    detail: &str,
) -> TcoLine {
    let mut basis = Vec::new();
    collect_basis(input, &mut basis);
    match input.value.clone() {
        Availability::Known { value } => match compute(value) {
            Ok(amount) => TcoLine {
                term,
                amount: Availability::known(amount),
                not_applicable: false,
                basis,
                detail: detail.to_string(),
            },
            Err(error) => TcoLine {
                term,
                amount: Availability::unavailable(UnavailableReason::Arithmetic, error.to_string()),
                not_applicable: false,
                basis,
                detail: detail.to_string(),
            },
        },
        Availability::Unavailable { unavailable } if unavailable.is_not_applicable() => TcoLine {
            term,
            amount: Availability::known(Money::zero(currency)),
            not_applicable: true,
            basis,
            detail: format!("{detail} (not applicable)"),
        },
        Availability::Unavailable { unavailable } => TcoLine {
            term,
            amount: Availability::Unavailable { unavailable },
            not_applicable: false,
            basis,
            detail: detail.to_string(),
        },
    }
}

fn sum_tco(lines: &[TcoLine]) -> Result<Availability<Money>, TcoError> {
    let mut total: Option<Money> = None;
    for line in lines {
        match &line.amount {
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
        None => Availability::unavailable(
            UnavailableReason::MissingEvidence,
            "no TCO terms".to_string(),
        ),
    })
}

/// Convenience: create an unknown TCO term.
pub fn unknown_term<T>(detail: impl Into<String>) -> Term<T> {
    Term::unknown(UnavailableReason::MissingEvidence, detail)
}

/// Convenience: create an explicit not-applicable TCO term.
pub fn not_applicable_term<T>(detail: impl Into<String>) -> Term<T> {
    Term::not_applicable(detail)
}

/// Convenience: a typed assumption.
pub fn assumption(
    id: &str,
    statement: &str,
    rationale: &str,
    provenance: faktor_supply_core::Provenance,
    falsifier: &str,
) -> Result<Assumption, ExactError> {
    Assumption::new(id, statement, rationale, provenance, falsifier)
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_supply_core::{Day, EvidenceRef, Provenance, Revision, SourceKind};

    const CUR: Currency = Currency::USD;

    fn provenance(id: &str) -> Provenance {
        Provenance::new(
            SourceKind::SupplierDocument,
            EvidenceRef::new(id, Revision(2), Some("digest")).unwrap(),
            Day(20_000),
            "fixture",
            vec!["quote revision 2".to_string()],
            "a revised quote with a different value",
        )
        .unwrap()
    }

    fn term<T>(value: T, id: &str) -> Term<T> {
        Term::evidence(value, provenance(id))
    }

    fn money(amount: &str) -> Money {
        Money::parse(amount, CUR).unwrap()
    }

    fn basic_input() -> ShouldCostInput {
        ShouldCostInput {
            currency: CUR,
            materials: vec![term(
                MaterialLine {
                    description: "6061-T6 bar".to_string(),
                    quantity_per_unit: ScaledQuantity::new(2_500_000).unwrap(),
                    unit_price: money("4.00"),
                    yield_rate: Rate::new(800_000).unwrap(),
                },
                "material",
            )],
            process_steps: vec![ProcessStep {
                operation: "CNC".to_string(),
                machine: term(
                    MachineTime {
                        rate_per_hour: money("120.00"),
                        cycle_seconds: 90,
                    },
                    "machine",
                ),
                setup: term(
                    SetupCost {
                        setup_cost: money("600.00"),
                        lot_size: Units::new(1_000).unwrap(),
                    },
                    "setup",
                ),
                tooling: term(
                    ToolingAmortization {
                        tooling_cost: money("12000.00"),
                        amortization_units: Units::new(30_000).unwrap(),
                    },
                    "tooling",
                ),
                labor: term(
                    LaborTime {
                        rate_per_hour: money("30.00"),
                        minutes_per_unit: 3,
                    },
                    "labor",
                ),
                yield_rate: term(Rate::new(950_000).unwrap(), "route-yield"),
            }],
            inspection: term(money("0.25"), "inspection"),
            scrap_rework: term(
                ScrapRework {
                    scrap_rate: Rate::new(20_000).unwrap(),
                    rework_rate: Rate::new(300_000).unwrap(),
                    rework_cost_per_unit: money("0.10"),
                },
                "scrap",
            ),
            overhead: term(
                Overhead {
                    rate: Rate::new(100_000).unwrap(),
                    base: OverheadBase::Conversion,
                },
                "overhead",
            ),
            packaging: term(money("0.20"), "packaging"),
            margin: term(Rate::new(250_000).unwrap(), "margin"),
            margin_mode: MarginMode::MarginOnPrice,
        }
    }

    fn basic_tco() -> TcoInput {
        TcoInput {
            currency: CUR,
            quantity: Units::new(1_000).unwrap(),
            ex_works_unit: term(money("10.00"), "exw"),
            origin_inland_freight: term(money("500.00"), "origin"),
            export_handling: term(money("100.00"), "export"),
            international_freight: term(money("2000.00"), "freight"),
            insurance: term(
                Insurance {
                    insured_value: money("12600.00"),
                    rate: Rate::new(5_000).unwrap(),
                    minimum_premium: money("25.00"),
                },
                "insurance",
            ),
            import_handling: term(money("150.00"), "import"),
            duty: term(
                Duty {
                    customs_value: money("12600.00"),
                    rate: Rate::new(50_000).unwrap(),
                    fixed: Money::zero(CUR),
                },
                "duty",
            ),
            tax: term(
                Tax {
                    taxable_value: money("13230.00"),
                    rate: Rate::new(190_000).unwrap(),
                    recoverable: false,
                },
                "tax",
            ),
            brokerage: term(money("80.00"), "brokerage"),
            final_mile: term(money("200.00"), "final-mile"),
            working_capital: term(
                WorkingCapital {
                    annual_rate: Rate::new(80_000).unwrap(),
                    days: 60,
                    financed_amount: money("10000.00"),
                },
                "capital",
            ),
            expected_quality: term(
                QualityCost {
                    defect_ppm: 500,
                    units: Units::new(1_000).unwrap(),
                    cost_per_defect: money("2.00"),
                },
                "quality",
            ),
            expected_expedite: term(
                ExpediteCost {
                    probability: Rate::new(100_000).unwrap(),
                    cost: money("500.00"),
                },
                "expedite",
            ),
            excess_moq_carrying: term(
                ExcessMoq {
                    excess_units: Units::new(200).unwrap(),
                    unit_value: money("10.00"),
                    annual_carrying_rate: Rate::new(60_000).unwrap(),
                    days: 90,
                },
                "excess",
            ),
            obsolescence: term(
                Obsolescence {
                    exposure_rate: Rate::new(50_000).unwrap(),
                    inventory_value: money("500.00"),
                },
                "obsolescence",
            ),
        }
    }

    #[test]
    fn should_cost_worked_numeric_case() {
        let result = should_cost(&basic_input()).unwrap();
        assert_eq!(
            result.ex_works.as_known().unwrap().to_string(),
            "26.292284 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Material)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "12.500000 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Machine)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "3.157895 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Setup)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "0.631579 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Tooling)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "0.421053 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Labor)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "1.578948 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Inspection)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "0.250000 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Scrap)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "0.370790 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Rework)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "0.030000 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Overhead)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "0.578948 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Packaging)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "0.200000 USD"
        );
        assert_eq!(
            result
                .line(CostTerm::Margin)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "6.573071 USD"
        );
        assert!(result.unavailable_terms.is_empty());
        let sum = result
            .lines
            .iter()
            .map(|line| line.amount.as_known().unwrap().micros())
            .sum::<i64>();
        assert_eq!(sum, result.ex_works.as_known().unwrap().micros());
    }

    #[test]
    fn missing_material_term_makes_total_unavailable_not_zero() {
        let mut input = basic_input();
        input.materials[0] = Term::unknown(
            UnavailableReason::NotProvided,
            "supplier did not state yield",
        );
        let result = should_cost(&input).unwrap();
        assert!(result.ex_works.as_known().is_none());
        assert!(result.unavailable_terms.contains(&CostTerm::Material));
        assert!(result.line(CostTerm::Machine).unwrap().amount.is_known());
    }

    #[test]
    fn zero_yield_is_typed_arithmetic_not_infinite_cost() {
        let mut input = basic_input();
        input.materials[0] = term(
            MaterialLine {
                description: "bad".to_string(),
                quantity_per_unit: ScaledQuantity::new(1_000_000).unwrap(),
                unit_price: money("1.00"),
                yield_rate: Rate::ZERO,
            },
            "bad-material",
        );
        assert!(matches!(
            should_cost(&input),
            Err(CostError::ZeroQuantity("process yield")) | Err(CostError::ZeroQuantity(_)) | Ok(_)
        ));
        // Material yield zero is only discovered in the arithmetic kernel; the
        // input validation does not invent a useful value.
        let mut manual = basic_input();
        if let Some(material) = manual.materials[0].value.as_known().cloned() {
            let mut material = material;
            material.yield_rate = Rate::ZERO;
            manual.materials[0] = term(material, "zero-yield");
        }
        let result = should_cost(&manual).unwrap();
        assert!(result.ex_works.as_known().is_none());
        assert!(matches!(
            result.line(CostTerm::Material).unwrap().amount,
            Availability::Unavailable { .. }
        ));
    }

    #[test]
    fn tooling_amortization_and_setup_are_exact() {
        let input = basic_input();
        let result = should_cost(&input).unwrap();
        let tooling = result.line(CostTerm::Tooling).unwrap();
        assert_eq!(tooling.amount.as_known().unwrap().micros(), 421_053);
        let setup = result.line(CostTerm::Setup).unwrap();
        assert_eq!(setup.amount.as_known().unwrap().micros(), 631_579);
    }

    #[test]
    fn markup_and_margin_on_price_are_distinct() {
        let mut markup = basic_input();
        markup.margin_mode = MarginMode::MarkupOnCost;
        markup.margin = term(Rate::new(250_000).unwrap(), "margin");
        let markup_result = should_cost(&markup).unwrap();
        // Cost before margin is 19.719213; markup 25% -> price 24.649016.
        assert_eq!(
            markup_result.ex_works.as_known().unwrap().to_string(),
            "24.649016 USD"
        );
        let margin_result = should_cost(&basic_input()).unwrap();
        assert_eq!(
            margin_result.ex_works.as_known().unwrap().to_string(),
            "26.292284 USD"
        );
        assert!(
            margin_result.ex_works.as_known().unwrap().micros()
                > markup_result.ex_works.as_known().unwrap().micros()
        );
    }

    #[test]
    fn currency_mismatch_is_refused() {
        let mut input = basic_input();
        input.packaging = term(
            Money::parse("0.20", Currency::EUR).unwrap(),
            "eur-packaging",
        );
        assert!(matches!(
            should_cost(&input),
            Err(CostError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn empty_routes_are_refused() {
        let mut input = basic_input();
        input.process_steps.clear();
        assert!(matches!(
            should_cost(&input),
            Err(CostError::NoProcessSteps)
        ));
        input = basic_input();
        input.materials.clear();
        assert!(matches!(should_cost(&input), Err(CostError::NoMaterials)));
    }

    #[test]
    fn tco_worked_numeric_case() {
        let result = tco(&basic_tco()).unwrap();
        assert_eq!(
            result
                .line(TcoTerm::ExWorks)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "10000.000000 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::Insurance)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "63.000000 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::Duty)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "630.000000 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::Tax)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "2513.700000 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::WorkingCapital)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "131.506849 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::ExpectedQuality)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "1.000000 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::ExpectedExpedite)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "50.000000 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::ExcessMoqCarrying)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "29.589041 USD"
        );
        assert_eq!(
            result
                .line(TcoTerm::Obsolescence)
                .unwrap()
                .amount
                .as_known()
                .unwrap()
                .to_string(),
            "25.000000 USD"
        );
        assert!(result.total.is_known());
        assert!(result.per_unit.is_known());
        let total = result.total.as_known().unwrap().micros();
        let per_unit = result.per_unit.as_known().unwrap().micros();
        assert_eq!(
            per_unit,
            faktor_supply_core::div_round_half_away(i128::from(total), 1000) as i64
        );
    }

    #[test]
    fn missing_tco_term_keeps_total_unknown_and_names_it() {
        let mut input = basic_tco();
        input.international_freight =
            Term::unknown(UnavailableReason::NotProvided, "no freight quote");
        let result = tco(&input).unwrap();
        assert!(!result.total.is_known());
        assert!(!result.per_unit.is_known());
        assert_eq!(
            result.unavailable_terms,
            vec![TcoTerm::InternationalFreight]
        );
        assert!(result.line(TcoTerm::Duty).unwrap().amount.is_known());
        assert!(format!(
            "{:?}",
            result.line(TcoTerm::InternationalFreight).unwrap().amount
        )
        .contains("no freight quote"));
    }

    #[test]
    fn recoverable_tax_is_zero_but_flagged_and_known() {
        let mut input = basic_tco();
        if let Some(tax) = input.tax.value.as_known().cloned() {
            let mut tax = tax;
            tax.recoverable = true;
            input.tax = term(tax, "tax-recoverable");
        }
        let result = tco(&input).unwrap();
        let tax = result.line(TcoTerm::Tax).unwrap();
        assert!(tax.amount.is_known());
        assert!(tax.amount.as_known().unwrap().is_zero());
        assert!(!tax.not_applicable);
    }

    #[test]
    fn explicitly_not_applicable_term_is_zero_and_flagged() {
        let mut input = basic_tco();
        input.duty = Term::not_applicable("domestic goods, no duty");
        input.insurance = Term::not_applicable("buyer self-insures");
        let result = tco(&input).unwrap();
        assert!(result.total.is_known());
        assert!(result.line(TcoTerm::Duty).unwrap().not_applicable);
        assert!(result.line(TcoTerm::Insurance).unwrap().not_applicable);
        assert!(result
            .line(TcoTerm::Duty)
            .unwrap()
            .amount
            .as_known()
            .unwrap()
            .is_zero());
    }

    #[test]
    fn unknown_term_is_not_the_same_as_not_applicable() {
        let mut input = basic_tco();
        input.brokerage = Term::unknown(UnavailableReason::MissingEvidence, "no brokerage quote");
        let result = tco(&input).unwrap();
        assert!(!result.total.is_known());
        assert!(!result.line(TcoTerm::Brokerage).unwrap().not_applicable);
        assert_eq!(result.unavailable_terms, vec![TcoTerm::Brokerage]);
    }

    #[test]
    fn tco_currency_mismatch_is_refused() {
        let mut input = basic_tco();
        input.final_mile = term(
            Money::parse("1.00", Currency::EUR).unwrap(),
            "eur-final-mile",
        );
        assert!(matches!(
            tco(&input),
            Err(TcoError::CurrencyMismatch { .. })
        ));
    }

    #[test]
    fn zero_quantity_is_refused() {
        let mut input = basic_tco();
        input.quantity = Units::ZERO;
        assert!(matches!(tco(&input), Err(TcoError::ZeroQuantity)));
    }

    #[test]
    fn negative_cost_is_refused() {
        let mut input = basic_tco();
        input.brokerage = term(money("-1.00"), "negative-brokerage");
        assert!(matches!(tco(&input), Err(TcoError::NegativeAmount(_))));
    }

    #[test]
    fn every_known_line_carries_a_basis() {
        let result = tco(&basic_tco()).unwrap();
        for line in &result.lines {
            assert!(line.amount.is_known());
            if !line.not_applicable {
                assert!(!line.basis.is_empty(), "{:?} has no basis", line.term);
            }
        }
    }

    #[test]
    fn should_cost_known_lines_carry_basis() {
        let result = should_cost(&basic_input()).unwrap();
        for line in &result.lines {
            assert!(line.amount.is_known());
            assert!(!line.basis.is_empty(), "{:?} has no basis", line.term);
        }
    }

    #[test]
    fn json_fixture_runs_the_same_exact_kernel() {
        let input: ShouldCostInput =
            serde_json::from_str(include_str!("../tests/fixtures/should_cost.json")).unwrap();
        let result = should_cost(&input).unwrap();
        assert_eq!(result.ex_works.as_known().unwrap().micros(), 26_292_284);
        assert!(result.unavailable_terms.is_empty());
    }

    #[test]
    fn assumption_propagates_to_the_result() {
        let mut input = basic_input();
        let assumption = Assumption::new(
            "a-freight",
            "freight holds",
            "carrier quote",
            provenance("freight-quote"),
            "carrier re-quote",
        )
        .unwrap();
        input.packaging = Term::assumption(money("0.20"), assumption);
        let result = should_cost(&input).unwrap();
        let packaging = result.line(CostTerm::Packaging).unwrap();
        assert!(matches!(packaging.basis[0], Basis::Assumption(_)));
    }
}
