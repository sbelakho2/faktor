//! faktor-supply-logistics — operational Incoterms, physical/logistics
//! arithmetic, and a customs framework that refuses to guess.
//!
//! # Incoterms 2020 semantics
//!
//! [`IncotermSemantics`] assigns each cost leg to the seller or the buyer and
//! states where risk transfers. It is a model of the published rule set, not a
//! legal opinion; transport mode compatibility is checked (the maritime terms
//! are refused for air/road/rail/courier).
//!
//! # Chargeable weight
//!
//! ```text
//! volume_cm3        = length_mm * width_mm * height_mm / 1000
//! volumetric_grams  = ceil(volume_cm3 * 1000 / divisor_cm3_per_kg)
//! chargeable_grams  = max(actual_grams, volumetric_grams) * package_count
//! ```
//!
//! # Customs
//!
//! Identity/composition/function -> candidate HS classifications, each with
//! General Interpretive Rule provenance -> only a unique viable candidate is
//! classified. Zero or multiple viable candidates, or missing product facts,
//! produce a typed escalation for a ruling; duty/tax arithmetic then refuses to
//! run on an unfinalized classification. Tariff rates are authoritative
//! inputs with evidence and revisions.

use std::collections::BTreeMap;

use faktor_supply_core::{
    validate_text, Availability, Currency, Day, Distribution, EvidenceRef, ExactError, Incoterm,
    Money, Provenance, Quantile, Rate, Revision, Term, TransportMode, UnavailableReason,
    MAX_ID_BYTES, MAX_TEXT_BYTES,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Maximum packages in one chargeable-weight request.
pub const MAX_PACKAGES: usize = 10_000;
/// Maximum shipment groups in one consolidation.
pub const MAX_CONSOLIDATIONS: usize = 100_000;
/// Maximum candidate HS classifications.
pub const MAX_HS_CANDIDATES: usize = 256;
/// Maximum criteria per candidate.
pub const MAX_CRITERIA: usize = 64;
/// Maximum GIR rules per candidate.
pub const MAX_GIR_RULES: usize = 16;
/// Maximum tariff entries.
pub const MAX_TARIFF_ENTRIES: usize = 100_000;
/// Maximum mode options.
pub const MAX_MODE_OPTIONS: usize = 16;
/// Maximum products facts in composition.
pub const MAX_COMPOSITION: usize = 64;
/// Maximum weight in grams (one billion tonnes).
pub const MAX_WEIGHT_G: u64 = 1_000_000_000_000_000;

/// A refused logistics operation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogisticsError {
    /// Exact arithmetic or value validation failed.
    #[error(transparent)]
    Exact(#[from] ExactError),
    /// A dimension or divisor is zero.
    #[error("zero {0}")]
    Zero(&'static str),
    /// The input exceeds a documented bound.
    #[error("{what} exceeds {max} entries")]
    Bound {
        /// The input class.
        what: &'static str,
        /// The maximum.
        max: usize,
    },
    /// A maritime Incoterm was paired with a non-maritime mode.
    #[error("{incoterm:?} requires sea or inland-water transport, not {mode:?}")]
    ModeIncompatible {
        /// The Incoterm.
        incoterm: Incoterm,
        /// The mode.
        mode: TransportMode,
    },
    /// No mode option meets the deadline.
    #[error("no mode option meets the {deadline_days}-day deadline")]
    NoFeasibleMode {
        /// The deadline.
        deadline_days: u32,
    },
    /// Duty was requested without a finalized classification.
    #[error("classification is not final: {0}")]
    ClassificationNotFinal(String),
    /// A "classified" document contradicts its own evaluation trail.
    #[error("classification is internally inconsistent: {0}")]
    ClassificationInconsistent(String),
    /// A tariff entry does not match the country/code.
    #[error("no tariff entry for {country} / {code}")]
    MissingTariffEntry {
        /// Destination country.
        country: String,
        /// HS code.
        code: String,
    },
    /// A tariff or origin revision is missing.
    #[error("tariff/origin evidence unavailable: {0}")]
    Unavailable(String),
    /// A currency mismatch.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch {
        /// Left.
        left: Currency,
        /// Right.
        right: Currency,
    },
    /// A negative amount.
    #[error("{0} must not be negative")]
    Negative(&'static str),
}

// ------------------------------------------------------------- incoterms

/// One cost leg in the international sale of goods.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CostLeg {
    /// Export packaging.
    ExportPackaging,
    /// Loading at origin.
    LoadingAtOrigin,
    /// Origin inland freight.
    OriginInlandFreight,
    /// Export clearance.
    ExportClearance,
    /// Main international carriage.
    MainCarriage,
    /// Cargo insurance.
    CargoInsurance,
    /// Import clearance.
    ImportClearance,
    /// Duty and import taxes.
    DutyAndTax,
    /// Destination inland freight.
    DestinationInlandFreight,
    /// Unloading at destination.
    UnloadingAtDestination,
}

impl CostLeg {
    /// All legs in operational order.
    pub const ALL: [CostLeg; 10] = [
        CostLeg::ExportPackaging,
        CostLeg::LoadingAtOrigin,
        CostLeg::OriginInlandFreight,
        CostLeg::ExportClearance,
        CostLeg::MainCarriage,
        CostLeg::CargoInsurance,
        CostLeg::ImportClearance,
        CostLeg::DutyAndTax,
        CostLeg::DestinationInlandFreight,
        CostLeg::UnloadingAtDestination,
    ];

    /// Stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExportPackaging => "export_packaging",
            Self::LoadingAtOrigin => "loading_at_origin",
            Self::OriginInlandFreight => "origin_inland_freight",
            Self::ExportClearance => "export_clearance",
            Self::MainCarriage => "main_carriage",
            Self::CargoInsurance => "cargo_insurance",
            Self::ImportClearance => "import_clearance",
            Self::DutyAndTax => "duty_and_tax",
            Self::DestinationInlandFreight => "destination_inland_freight",
            Self::UnloadingAtDestination => "unloading_at_destination",
        }
    }
}

/// Where risk passes from seller to buyer.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTransferPoint {
    /// At the seller's premises.
    SellerPremises,
    /// On delivery to the buyer's nominated carrier.
    Carrier,
    /// Alongside the ship.
    AlongsideShip,
    /// On board the ship.
    OnBoard,
    /// At the named destination, ready for unloading.
    DestinationReadyForUnloading,
    /// At the named destination, unloaded.
    DestinationUnloaded,
    /// At the named destination, imported and cleared.
    DestinationCleared,
}

/// The seller's insurance obligation under the term.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsuranceObligation {
    /// No seller insurance obligation.
    None,
    /// Minimum cover of 110% of the contract value (CIF/CIP).
    Minimum110Percent,
}

/// Incoterm operational semantics (Incoterms 2020 model).
pub trait IncotermSemantics {
    /// True when the seller bears the cost of this leg.
    fn seller_pays(&self, leg: CostLeg) -> bool;
    /// Where risk transfers.
    fn risk_transfer(&self) -> RiskTransferPoint;
    /// Seller insurance obligation.
    fn insurance_obligation(&self) -> InsuranceObligation;
    /// True when the term is only meaningful for sea/inland water.
    fn is_maritime_term(&self) -> bool;
}

impl IncotermSemantics for Incoterm {
    fn seller_pays(&self, leg: CostLeg) -> bool {
        use CostLeg::{
            CargoInsurance, DestinationInlandFreight, DutyAndTax, ExportClearance, ExportPackaging,
            ImportClearance, LoadingAtOrigin, MainCarriage, OriginInlandFreight,
            UnloadingAtDestination,
        };
        match self {
            Incoterm::Exw => matches!(leg, ExportPackaging),
            Incoterm::Fca => matches!(
                leg,
                ExportPackaging | LoadingAtOrigin | OriginInlandFreight | ExportClearance
            ),
            Incoterm::Fas => matches!(leg, ExportPackaging | OriginInlandFreight | ExportClearance),
            Incoterm::Fob => matches!(
                leg,
                ExportPackaging | OriginInlandFreight | ExportClearance | LoadingAtOrigin
            ),
            Incoterm::Cfr => matches!(
                leg,
                ExportPackaging
                    | LoadingAtOrigin
                    | OriginInlandFreight
                    | ExportClearance
                    | MainCarriage
            ),
            Incoterm::Cif => matches!(
                leg,
                ExportPackaging
                    | LoadingAtOrigin
                    | OriginInlandFreight
                    | ExportClearance
                    | MainCarriage
                    | CargoInsurance
            ),
            Incoterm::Cpt => matches!(
                leg,
                ExportPackaging
                    | LoadingAtOrigin
                    | OriginInlandFreight
                    | ExportClearance
                    | MainCarriage
            ),
            Incoterm::Cip => matches!(
                leg,
                ExportPackaging
                    | LoadingAtOrigin
                    | OriginInlandFreight
                    | ExportClearance
                    | MainCarriage
                    | CargoInsurance
            ),
            Incoterm::Dap => matches!(
                leg,
                ExportPackaging
                    | LoadingAtOrigin
                    | OriginInlandFreight
                    | ExportClearance
                    | MainCarriage
                    | DestinationInlandFreight
            ),
            Incoterm::Dpu => matches!(
                leg,
                ExportPackaging
                    | LoadingAtOrigin
                    | OriginInlandFreight
                    | ExportClearance
                    | MainCarriage
                    | DestinationInlandFreight
                    | UnloadingAtDestination
            ),
            Incoterm::Ddp => matches!(
                leg,
                ExportPackaging
                    | LoadingAtOrigin
                    | OriginInlandFreight
                    | ExportClearance
                    | MainCarriage
                    | ImportClearance
                    | DutyAndTax
                    | DestinationInlandFreight
            ),
        }
    }

    fn risk_transfer(&self) -> RiskTransferPoint {
        match self {
            Incoterm::Exw => RiskTransferPoint::SellerPremises,
            Incoterm::Fca | Incoterm::Cpt | Incoterm::Cip => RiskTransferPoint::Carrier,
            Incoterm::Fas => RiskTransferPoint::AlongsideShip,
            Incoterm::Fob | Incoterm::Cfr | Incoterm::Cif => RiskTransferPoint::OnBoard,
            Incoterm::Dap => RiskTransferPoint::DestinationReadyForUnloading,
            Incoterm::Dpu => RiskTransferPoint::DestinationUnloaded,
            Incoterm::Ddp => RiskTransferPoint::DestinationCleared,
        }
    }

    fn insurance_obligation(&self) -> InsuranceObligation {
        match self {
            Incoterm::Cif | Incoterm::Cip => InsuranceObligation::Minimum110Percent,
            _ => InsuranceObligation::None,
        }
    }

    fn is_maritime_term(&self) -> bool {
        matches!(
            self,
            Incoterm::Fas | Incoterm::Fob | Incoterm::Cfr | Incoterm::Cif
        )
    }
}

/// Validate the term against the transport mode.
pub fn validate_term_for_mode(
    incoterm: Incoterm,
    mode: TransportMode,
) -> Result<(), LogisticsError> {
    if incoterm.is_maritime_term()
        && !matches!(mode, TransportMode::OceanFcl | TransportMode::OceanLcl)
    {
        return Err(LogisticsError::ModeIncompatible { incoterm, mode });
    }
    Ok(())
}

/// A freight/cost estimate for one leg, bound to evidence.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegEstimate {
    /// The leg.
    pub leg: CostLeg,
    /// Estimated amount.
    pub amount: Term<Money>,
}

/// Who bears each leg under a term, and the exact buyer-borne total when every
/// buyer leg is estimated.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegAllocation {
    /// The term.
    pub incoterm: Incoterm,
    /// Seller-borne legs.
    pub seller_legs: Vec<CostLeg>,
    /// Buyer-borne legs.
    pub buyer_legs: Vec<CostLeg>,
    /// Buyer-borne cost, when every buyer leg is known.
    pub buyer_total: Availability<Money>,
    /// Buyer legs with no estimate (never treated as zero).
    #[serde(default)]
    pub missing_legs: Vec<CostLeg>,
    /// Risk transfer point.
    pub risk_transfer: RiskTransferPoint,
    /// Insurance obligation.
    pub insurance: InsuranceObligation,
}

/// Allocate legs under an Incoterm. Sellers are not asked for estimates of
/// legs they bear: those costs are inside the quoted price under that term.
pub fn allocate_legs(
    incoterm: Incoterm,
    currency: Currency,
    estimates: &[LegEstimate],
) -> Result<LegAllocation, LogisticsError> {
    let mut seller_legs = Vec::new();
    let mut buyer_legs = Vec::new();
    let mut total = Money::zero(currency);
    let mut missing = Vec::new();
    for leg in CostLeg::ALL {
        if incoterm.seller_pays(leg) {
            seller_legs.push(leg);
        } else {
            buyer_legs.push(leg);
            let estimate = estimates.iter().find(|estimate| estimate.leg == leg);
            match estimate.map(|estimate| &estimate.amount.value) {
                Some(Availability::Known { value }) => {
                    if value.currency != currency {
                        return Err(LogisticsError::CurrencyMismatch {
                            left: currency,
                            right: value.currency,
                        });
                    }
                    if value.micros() < 0 {
                        return Err(LogisticsError::Negative("freight estimate"));
                    }
                    total = total.checked_add(*value)?;
                }
                Some(Availability::Unavailable { unavailable })
                    if unavailable.is_not_applicable() => {}
                Some(Availability::Unavailable { .. }) | None => missing.push(leg),
            }
        }
    }
    let buyer_total = if missing.is_empty() {
        Availability::Known { value: total }
    } else {
        Availability::unavailable(
            UnavailableReason::MissingEvidence,
            format!(
                "missing buyer leg estimates: {}",
                missing
                    .iter()
                    .map(|leg| leg.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    };
    Ok(LegAllocation {
        incoterm,
        seller_legs,
        buyer_legs,
        buyer_total,
        missing_legs: missing,
        risk_transfer: incoterm.risk_transfer(),
        insurance: incoterm.insurance_obligation(),
    })
}

// ---------------------------------------------------------- physical cargo

/// A rectangular package.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    /// Length in millimetres.
    pub length_mm: u64,
    /// Width in millimetres.
    pub width_mm: u64,
    /// Height in millimetres.
    pub height_mm: u64,
    /// Actual gross weight in grams.
    pub actual_weight_g: u64,
    /// Number of identical packages.
    pub quantity: u64,
}

impl Package {
    /// Validate dimensions and quantity.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        if self.length_mm == 0 || self.width_mm == 0 || self.height_mm == 0 {
            return Err(LogisticsError::Zero("package dimension"));
        }
        if self.quantity == 0 {
            return Err(LogisticsError::Zero("package quantity"));
        }
        if self.actual_weight_g > MAX_WEIGHT_G {
            return Err(LogisticsError::Bound {
                what: "package weight",
                max: MAX_WEIGHT_G as usize,
            });
        }
        Ok(())
    }

    /// Volume in cubic centimetres (rounded up; volume is never understated
    /// for chargeable-weight purposes).
    pub fn volume_cm3(&self) -> Result<u64, LogisticsError> {
        let mm3 = u128::from(self.length_mm)
            .checked_mul(u128::from(self.width_mm))
            .and_then(|value| value.checked_mul(u128::from(self.height_mm)))
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?;
        let cm3 = mm3
            .checked_add(999)
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?
            / 1_000;
        u64::try_from(cm3).map_err(|_| LogisticsError::Exact(ExactError::Overflow))
    }
}

/// The weight computation breakdown.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChargeableWeight {
    /// Actual weight in grams (all packages).
    pub actual_g: u64,
    /// Volumetric weight in grams (all packages).
    pub volumetric_g: u64,
    /// Chargeable weight in grams.
    pub chargeable_g: u64,
    /// Packages counted.
    pub packages: u64,
}

/// Compute chargeable weight. The divisor is cm3 per kg (6000 for many
/// air tariffs, 5000 for the common road/express convention).
pub fn chargeable_weight(
    packages: &[Package],
    divisor_cm3_per_kg: u64,
) -> Result<ChargeableWeight, LogisticsError> {
    if divisor_cm3_per_kg == 0 {
        return Err(LogisticsError::Zero("volumetric divisor"));
    }
    if packages.len() > MAX_PACKAGES {
        return Err(LogisticsError::Bound {
            what: "packages",
            max: MAX_PACKAGES,
        });
    }
    let mut actual = 0_u128;
    let mut volumetric = 0_u128;
    let mut count = 0_u128;
    for package in packages {
        package.validate()?;
        let volume = u128::from(package.volume_cm3()?);
        let volumetric_g = volume
            .checked_mul(1_000)
            .and_then(|value| value.checked_add(u128::from(divisor_cm3_per_kg) - 1))
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?
            / u128::from(divisor_cm3_per_kg);
        actual = actual
            .checked_add(
                u128::from(package.actual_weight_g)
                    .checked_mul(u128::from(package.quantity))
                    .ok_or(LogisticsError::Exact(ExactError::Overflow))?,
            )
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?;
        volumetric = volumetric
            .checked_add(
                volumetric_g
                    .checked_mul(u128::from(package.quantity))
                    .ok_or(LogisticsError::Exact(ExactError::Overflow))?,
            )
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?;
        count = count
            .checked_add(u128::from(package.quantity))
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?;
    }
    let chargeable = actual.max(volumetric);
    let actual_g =
        u64::try_from(actual).map_err(|_| LogisticsError::Exact(ExactError::Overflow))?;
    let volumetric_g =
        u64::try_from(volumetric).map_err(|_| LogisticsError::Exact(ExactError::Overflow))?;
    let chargeable_g =
        u64::try_from(chargeable).map_err(|_| LogisticsError::Exact(ExactError::Overflow))?;
    if chargeable_g > MAX_WEIGHT_G {
        return Err(LogisticsError::Bound {
            what: "chargeable weight",
            max: MAX_WEIGHT_G as usize,
        });
    }
    Ok(ChargeableWeight {
        actual_g,
        volumetric_g,
        chargeable_g,
        packages: u64::try_from(count).map_err(|_| LogisticsError::Exact(ExactError::Overflow))?,
    })
}

/// A consolidated lane group.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidationGroup {
    /// Origin.
    pub origin: String,
    /// Destination.
    pub destination: String,
    /// Mode.
    pub mode: TransportMode,
    /// Shipment ids in the group.
    pub shipments: Vec<String>,
    /// Total chargeable grams.
    pub chargeable_g: u64,
}

/// Group shipments by lane/mode for consolidation. Deterministic `BTreeMap`
/// ordering; `O(n log n)`.
pub fn consolidate(
    shipments: &[(String, String, String, TransportMode, u64)],
) -> Result<Vec<ConsolidationGroup>, LogisticsError> {
    if shipments.len() > MAX_CONSOLIDATIONS {
        return Err(LogisticsError::Bound {
            what: "shipments",
            max: MAX_CONSOLIDATIONS,
        });
    }
    let mut groups: BTreeMap<(String, String, TransportMode), (Vec<String>, u64)> = BTreeMap::new();
    for (id, origin, destination, mode, weight_g) in shipments {
        validate_text(id, MAX_ID_BYTES, "shipment id")?;
        validate_text(origin, MAX_TEXT_BYTES, "origin")?;
        validate_text(destination, MAX_TEXT_BYTES, "destination")?;
        let entry = groups
            .entry((origin.clone(), destination.clone(), *mode))
            .or_default();
        entry.0.push(id.clone());
        entry.1 = entry
            .1
            .checked_add(*weight_g)
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?;
    }
    let mut result = Vec::new();
    for ((origin, destination, mode), (mut ids, weight)) in groups {
        ids.sort();
        result.push(ConsolidationGroup {
            origin,
            destination,
            mode,
            shipments: ids,
            chargeable_g: weight,
        });
    }
    Ok(result)
}

/// One selectable transport mode.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeOption {
    /// Mode.
    pub mode: TransportMode,
    /// Transit-time distribution in days.
    pub transit_days: Term<Distribution>,
    /// Fixed charge.
    pub fixed_cost: Term<Money>,
    /// Rate per chargeable kg.
    pub cost_per_kg: Term<Money>,
    /// Minimum charge.
    pub minimum_charge: Term<Money>,
    /// Maximum weight the option can carry.
    pub max_weight_g: u64,
}

/// The selected mode and its exact cost.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeSelection {
    /// Selected mode.
    pub mode: TransportMode,
    /// Total logistics cost.
    pub cost: Money,
    /// P90 transit days.
    pub transit_days_p90: i64,
    /// Chargeable weight.
    pub chargeable_g: u64,
}

/// Select the cheapest feasible mode for a deadline, using P90 transit as the
/// planning duration. Ties break by mode label; no hidden weighting.
pub fn select_mode(
    options: &[ModeOption],
    chargeable_g: u64,
    deadline_days: u32,
) -> Result<Availability<ModeSelection>, LogisticsError> {
    if options.len() > MAX_MODE_OPTIONS {
        return Err(LogisticsError::Bound {
            what: "mode options",
            max: MAX_MODE_OPTIONS,
        });
    }
    let mut feasible = Vec::new();
    let mut missing = None;
    for option in options {
        if chargeable_g > option.max_weight_g {
            continue;
        }
        let transit = option.transit_days.as_known();
        let fixed = option.fixed_cost.as_known();
        let per_kg = option.cost_per_kg.as_known();
        let minimum = option.minimum_charge.as_known();
        let (Some(transit), Some(fixed), Some(per_kg), Some(minimum)) =
            (transit, fixed, per_kg, minimum)
        else {
            missing.get_or_insert(format!("{:?} has an unavailable term", option.mode));
            continue;
        };
        if transit.p90 > i64::from(deadline_days) {
            continue;
        }
        let kg_units = chargeable_g
            .checked_add(999)
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?
            / 1_000;
        let variable = per_kg.mul_units(kg_units)?;
        let mut cost = fixed.checked_add(variable)?;
        if cost.micros() < minimum.micros() {
            cost = *minimum;
        }
        feasible.push((cost, transit.p90, option.mode));
    }
    if feasible.is_empty() {
        return Ok(match missing {
            Some(detail) => Availability::unavailable(UnavailableReason::MissingEvidence, detail),
            None => Availability::unavailable(
                UnavailableReason::NoCandidate,
                format!("no mode meets the {deadline_days}-day deadline"),
            ),
        });
    }
    feasible.sort_by(|left, right| {
        left.0
            .micros()
            .cmp(&right.0.micros())
            .then(left.1.cmp(&right.1))
            .then(format!("{:?}", left.2).cmp(&format!("{:?}", right.2)))
    });
    let (cost, transit_days_p90, mode) = feasible.remove(0);
    Ok(Availability::known(ModeSelection {
        mode,
        cost,
        transit_days_p90,
        chargeable_g,
    }))
}

/// Handling charges.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlingCharge {
    /// Fixed charge per shipment.
    pub per_shipment: Money,
    /// Charge per chargeable kg (rounded up to whole kg).
    pub per_kg: Money,
}

impl HandlingCharge {
    /// Compute the handling cost.
    pub fn cost(&self, chargeable_g: u64) -> Result<Money, LogisticsError> {
        let kg_units = chargeable_g
            .checked_add(999)
            .ok_or(LogisticsError::Exact(ExactError::Overflow))?
            / 1_000;
        self.per_shipment
            .checked_add(self.per_kg.mul_units(kg_units)?)
            .map_err(LogisticsError::Exact)
    }
}

/// Demurrage / storage terms.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DemurrageTerms {
    /// Free days before charges begin.
    pub free_days: u32,
    /// Charge per day after free time.
    pub daily_rate: Money,
    /// Storage charge per day (in addition to demurrage), when applicable.
    pub storage_per_day: Term<Money>,
}

impl DemurrageTerms {
    /// Compute charges for a dwell duration. The dwell is explicit; an
    /// unavailable dwell distribution never becomes zero dwell.
    pub fn expected(
        &self,
        dwell_days: &Term<Distribution>,
        quantile: Quantile,
    ) -> Availability<Money> {
        let dwell = match dwell_days.as_known() {
            Some(dwell) => dwell,
            None => {
                return Availability::unavailable(
                    UnavailableReason::MissingEvidence,
                    "port dwell distribution unavailable",
                )
            }
        };
        if let Err(error) = dwell.validate() {
            return Availability::unavailable(UnavailableReason::OutOfRange, error.to_string());
        }
        let observed = dwell.quantile(quantile).max(0) as u64;
        let chargeable_days = observed.saturating_sub(u64::from(self.free_days));
        let demurrage = match self.daily_rate.mul_units(chargeable_days) {
            Ok(value) => value,
            Err(error) => {
                return Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
            }
        };
        match &self.storage_per_day.value {
            Availability::Known { value } => match value.mul_units(chargeable_days) {
                Ok(storage) => match demurrage.checked_add(storage) {
                    Ok(total) => Availability::Known { value: total },
                    Err(error) => {
                        Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
                    }
                },
                Err(error) => {
                    Availability::unavailable(UnavailableReason::Arithmetic, error.to_string())
                }
            },
            Availability::Unavailable { unavailable } if unavailable.is_not_applicable() => {
                Availability::Known { value: demurrage }
            }
            Availability::Unavailable { unavailable } => Availability::Unavailable {
                unavailable: unavailable.clone(),
            },
        }
    }
}

/// Insurance terms.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsuranceTerms {
    /// Premium rate on the insured value.
    pub rate: Rate,
    /// Minimum premium.
    pub minimum_premium: Money,
    /// Coverage as ppm of the insured value (110% -> 1_100_000; a rate above
    /// 100% is stored as parts per million of value, not `Rate`).
    pub coverage_ppm: i64,
}

impl InsuranceTerms {
    /// Premium for an insured value.
    pub fn premium(&self, insured_value: Money) -> Result<Money, LogisticsError> {
        if self.coverage_ppm < 0 {
            return Err(LogisticsError::Negative("coverage"));
        }
        let covered = Money::from_micros(
            i64::try_from(
                faktor_supply_core::mul_div_round_half_up(
                    i128::from(insured_value.micros()),
                    i128::from(self.coverage_ppm),
                    1_000_000,
                )
                .ok_or(LogisticsError::Exact(ExactError::Overflow))?,
            )
            .map_err(|_| LogisticsError::Exact(ExactError::Overflow))?,
            insured_value.currency,
        )?;
        let premium = covered.scale(self.rate)?;
        if premium.micros() < self.minimum_premium.micros() {
            Ok(self.minimum_premium)
        } else {
            Ok(premium)
        }
    }
}

/// Dangerous-goods declaration.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DangerousGoods {
    /// UN number (four digits, optional "UN" prefix).
    pub un_number: String,
    /// Hazard class.
    pub hazard_class: String,
    /// Packing group, when applicable.
    pub packing_group: Option<String>,
    /// Shipper's declaration evidence.
    pub declaration: EvidenceRef,
}

impl DangerousGoods {
    /// Validate the declaration.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        let digits = self.un_number.strip_prefix("UN").unwrap_or(&self.un_number);
        if digits.len() != 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(LogisticsError::Unavailable(
                "UN number must be four digits".to_string(),
            ));
        }
        validate_text(&self.hazard_class, MAX_ID_BYTES, "hazard class")?;
        if self.hazard_class.is_empty() {
            return Err(LogisticsError::Unavailable(
                "hazard class is empty".to_string(),
            ));
        }
        if let Some(group) = &self.packing_group {
            validate_text(group, MAX_ID_BYTES, "packing group")?;
        }
        self.declaration.validate()?;
        Ok(())
    }
}

// ---------------------------------------------------------------- customs

/// A declared material share of the product.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionShare {
    /// Material name.
    pub material: String,
    /// Share in parts-per-million (must be <= 1,000,000).
    pub share_ppm: i64,
}

/// Product facts used for classification.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductFacts {
    /// Description.
    pub description: String,
    /// Composition, when known. `None` means unknown, not empty.
    pub composition: Option<Vec<CompositionShare>>,
    /// Function, when known.
    pub function: Option<String>,
    /// Essential character, when known.
    pub essential_character: Option<String>,
}

impl ProductFacts {
    /// Validate bounds and composition sum.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        validate_text(&self.description, MAX_TEXT_BYTES, "product description")?;
        if let Some(composition) = &self.composition {
            if composition.len() > MAX_COMPOSITION {
                return Err(LogisticsError::Bound {
                    what: "composition",
                    max: MAX_COMPOSITION,
                });
            }
            let mut total = 0_i128;
            for share in composition {
                validate_text(&share.material, MAX_ID_BYTES, "composition material")?;
                if !(0..=1_000_000).contains(&share.share_ppm) {
                    return Err(LogisticsError::Unavailable(format!(
                        "composition share {} is outside 0..=1,000,000",
                        share.share_ppm
                    )));
                }
                total += i128::from(share.share_ppm);
            }
            if total > 1_000_000 {
                return Err(LogisticsError::Unavailable(
                    "composition shares exceed 100%".to_string(),
                ));
            }
        }
        if let Some(function) = &self.function {
            validate_text(function, MAX_TEXT_BYTES, "function")?;
        }
        if let Some(character) = &self.essential_character {
            validate_text(character, MAX_TEXT_BYTES, "essential character")?;
        }
        Ok(())
    }

    fn share(&self, material: &str) -> Option<i64> {
        // A known composition that does not list the material means its
        // declared share is zero, not unknown.
        self.composition.as_ref().map(|composition| {
            composition
                .iter()
                .find(|share| share.material.eq_ignore_ascii_case(material))
                .map_or(0, |share| share.share_ppm)
        })
    }
}

/// General Interpretive Rules 1-6.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GirRule {
    /// GIR 1: classification by the terms of the headings and section/chapter notes.
    G1,
    /// GIR 2(a): incomplete/unassembled articles with essential character.
    G2a,
    /// GIR 2(b): mixtures and combinations.
    G2b,
    /// GIR 3(a): most specific description.
    G3a,
    /// GIR 3(b): essential character.
    G3b,
    /// GIR 3(c): last in numerical order.
    G3c,
    /// GIR 4: goods most akin.
    G4,
    /// GIR 5: packing cases/containers.
    G5,
    /// GIR 6: subheading comparison at the same level.
    G6,
}

/// One interpretive-rule assertion, bound to provenance.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterpretiveRule {
    /// The GIR rule applied.
    pub rule: GirRule,
    /// Why the rule selects this candidate.
    pub note: String,
    /// The authority/provenance for the interpretation.
    pub provenance: Provenance,
}

impl InterpretiveRule {
    /// Validate bounds and provenance.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        validate_text(&self.note, MAX_TEXT_BYTES, "GIR note")?;
        self.provenance.validate()?;
        Ok(())
    }
}

/// A criterion a candidate must satisfy.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "criterion", rename_all = "snake_case", deny_unknown_fields)]
pub enum CandidateCriterion {
    /// A material share must be at least the given ppm.
    CompositionAtLeast {
        /// Material.
        material: String,
        /// Minimum share.
        min_ppm: i64,
    },
    /// A material share must be at most the given ppm.
    CompositionAtMost {
        /// Material.
        material: String,
        /// Maximum share.
        max_ppm: i64,
    },
    /// The function text must contain the given phrase.
    FunctionContains {
        /// Phrase.
        text: String,
    },
    /// The essential-character text must contain the given phrase.
    EssentialCharacterContains {
        /// Phrase.
        text: String,
    },
    /// The description must contain the given phrase.
    DescriptionContains {
        /// Phrase.
        text: String,
    },
}

impl CandidateCriterion {
    fn validate(&self) -> Result<(), LogisticsError> {
        match self {
            Self::CompositionAtLeast { material, min_ppm } => {
                validate_text(material, MAX_ID_BYTES, "criterion material")?;
                if !(0..=1_000_000).contains(min_ppm) {
                    return Err(LogisticsError::Unavailable(
                        "criterion share outside 0..=1,000,000".to_string(),
                    ));
                }
            }
            Self::CompositionAtMost { material, max_ppm } => {
                validate_text(material, MAX_ID_BYTES, "criterion material")?;
                if !(0..=1_000_000).contains(max_ppm) {
                    return Err(LogisticsError::Unavailable(
                        "criterion share outside 0..=1,000,000".to_string(),
                    ));
                }
            }
            Self::FunctionContains { text }
            | Self::EssentialCharacterContains { text }
            | Self::DescriptionContains { text } => {
                validate_text(text, MAX_TEXT_BYTES, "criterion text")?
            }
        }
        Ok(())
    }

    fn label(&self) -> String {
        match self {
            Self::CompositionAtLeast { material, min_ppm } => {
                format!("composition {material} >= {min_ppm} ppm")
            }
            Self::CompositionAtMost { material, max_ppm } => {
                format!("composition {material} <= {max_ppm} ppm")
            }
            Self::FunctionContains { text } => format!("function contains {text}"),
            Self::EssentialCharacterContains { text } => {
                format!("essential character contains {text}")
            }
            Self::DescriptionContains { text } => format!("description contains {text}"),
        }
    }
}

/// A candidate HS classification supplied by an authoritative input.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HsCandidate {
    /// HS code (6-10 digits).
    pub code: String,
    /// Candidate description.
    pub description: String,
    /// Criteria that must hold.
    #[serde(default)]
    pub criteria: Vec<CandidateCriterion>,
    /// Interpretive rules with provenance.
    pub gir: Vec<InterpretiveRule>,
    /// Authoritative source evidence for the candidate.
    pub evidence: EvidenceRef,
    /// Revision of the candidate list.
    pub revision: Revision,
}

impl HsCandidate {
    /// Validate code, bounds and GIR provenance.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        if !(6..=10).contains(&self.code.len())
            || !self.code.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(LogisticsError::Unavailable(format!(
                "HS code {} must be 6-10 digits",
                self.code
            )));
        }
        validate_text(&self.description, MAX_TEXT_BYTES, "HS description")?;
        if self.criteria.len() > MAX_CRITERIA {
            return Err(LogisticsError::Bound {
                what: "candidate criteria",
                max: MAX_CRITERIA,
            });
        }
        for criterion in &self.criteria {
            criterion.validate()?;
        }
        if self.gir.is_empty() || self.gir.len() > MAX_GIR_RULES {
            return Err(LogisticsError::Unavailable(
                "a candidate needs 1..=16 interpretive rules with provenance".to_string(),
            ));
        }
        for rule in &self.gir {
            rule.validate()?;
        }
        self.evidence.validate()?;
        Ok(())
    }
}

/// The outcome of candidate evaluation.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClassificationOutcome {
    /// Exactly one viable candidate.
    Classified {
        /// The code.
        code: String,
    },
    /// Several viable candidates; a ruling is required.
    Ambiguous {
        /// The viable codes.
        codes: Vec<String>,
    },
    /// Product facts are insufficient to decide.
    InsufficientFacts {
        /// Missing or unknown facts.
        missing: Vec<String>,
    },
    /// No candidate is viable.
    NoCandidate,
}

/// Why escalation is required.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationKind {
    /// A binding classification ruling is required.
    BindingRulingRequired,
    /// Product engineering data must be completed.
    ProductFactsRequired,
    /// The tariff input must be corrected.
    TariffInputRequired,
}

/// A typed escalation.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Escalation {
    /// Kind.
    pub kind: EscalationKind,
    /// Reason.
    pub reason: String,
    /// Authority to consult.
    pub authority: String,
}

/// One candidate evaluation.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEvaluation {
    /// Code.
    pub code: String,
    /// Criteria results.
    pub criteria: Vec<CriterionResult>,
    /// True only when every criterion is known and satisfied.
    pub viable: bool,
}

/// One criterion result.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionResult {
    /// The criterion description.
    pub criterion: String,
    /// `satisfied`, `violated`, or `unknown`.
    pub outcome: String,
    /// Explanation.
    pub detail: String,
}

/// Classification input.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassificationRequest {
    /// Product facts.
    pub facts: ProductFacts,
    /// Candidate classifications.
    pub candidates: Vec<HsCandidate>,
}

/// Classification result with audit trail.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    /// Canonical digest of the exact ProductFacts/candidates request that
    /// produced this classification. A human acceptance binds to it, so an
    /// approval for one product cannot be replayed against another product
    /// whose evaluations happen to look the same.
    #[serde(default)]
    pub request_digest: String,
    /// Outcome.
    pub outcome: ClassificationOutcome,
    /// Candidate evaluations.
    pub evaluations: Vec<CandidateEvaluation>,
    /// Escalation when the outcome is not `classified`.
    pub escalation: Option<Escalation>,
    /// Evidence referenced.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

impl Classification {
    /// The finalized code, if any.
    pub fn code(&self) -> Option<&str> {
        match &self.outcome {
            ClassificationOutcome::Classified { code } => Some(code),
            _ => None,
        }
    }
}

/// Classify a product by evaluating authoritative candidates. A candidate is
/// viable only when **all** its criteria are known and satisfied. Ambiguity,
/// missing facts and zero candidates are escalated, never guessed.
pub fn classify(request: &ClassificationRequest) -> Result<Classification, LogisticsError> {
    request.facts.validate()?;
    let request_digest = faktor_approval::content_digest(b"faktor.supply.hs-request/v1", request)
        .map_err(ExactError::Approval)?;
    if request.candidates.is_empty() {
        return Ok(Classification {
            request_digest,
            outcome: ClassificationOutcome::NoCandidate,
            evaluations: vec![],
            escalation: Some(Escalation {
                kind: EscalationKind::BindingRulingRequired,
                reason: "no candidate classifications supplied".to_string(),
                authority: "customs classification authority".to_string(),
            }),
            evidence: vec![],
        });
    }
    if request.candidates.len() > MAX_HS_CANDIDATES {
        return Err(LogisticsError::Bound {
            what: "HS candidates",
            max: MAX_HS_CANDIDATES,
        });
    }
    let mut evidence = Vec::new();
    let mut evaluations = Vec::new();
    let mut viable = Vec::new();
    let mut potentially_viable = Vec::new();
    for candidate in &request.candidates {
        candidate.validate()?;
        if !evidence.contains(&candidate.evidence) {
            evidence.push(candidate.evidence.clone());
        }
        for rule in &candidate.gir {
            if !evidence.contains(&rule.provenance.evidence) {
                evidence.push(rule.provenance.evidence.clone());
            }
        }
        let mut criterion_results = Vec::new();
        let mut all_satisfied = true;
        let mut candidate_unknown = false;
        let mut candidate_violated = false;
        for criterion in &candidate.criteria {
            let (outcome, detail) = evaluate_criterion(criterion, &request.facts);
            if outcome == "unknown" {
                candidate_unknown = true;
                all_satisfied = false;
            } else if outcome != "satisfied" {
                all_satisfied = false;
                candidate_violated = true;
            }
            criterion_results.push(CriterionResult {
                criterion: criterion.label(),
                outcome: outcome.to_string(),
                detail,
            });
        }
        let viable_candidate = all_satisfied && !candidate_unknown;
        if viable_candidate {
            viable.push(candidate.code.clone());
        } else if candidate_unknown && !candidate_violated {
            potentially_viable.push(candidate.code.clone());
        }
        evaluations.push(CandidateEvaluation {
            code: candidate.code.clone(),
            criteria: criterion_results,
            viable: viable_candidate,
        });
    }
    viable.sort();
    viable.dedup();
    let (outcome, escalation) = if viable.len() == 1 && !potentially_viable.is_empty() {
        let mut unresolved = potentially_viable;
        unresolved.sort();
        unresolved.dedup();
        (
            ClassificationOutcome::InsufficientFacts {
                missing: missing_facts(&request.facts, &request.candidates),
            },
            Some(Escalation {
                kind: EscalationKind::ProductFactsRequired,
                reason: format!(
                    "candidate {} is viable but {} remains undecidable",
                    viable[0],
                    unresolved.join(", ")
                ),
                authority: "product engineering / customs".to_string(),
            }),
        )
    } else if viable.len() == 1 {
        (
            ClassificationOutcome::Classified {
                code: viable[0].clone(),
            },
            None,
        )
    } else if viable.len() > 1 {
        (
            ClassificationOutcome::Ambiguous {
                codes: viable.clone(),
            },
            Some(Escalation {
                kind: EscalationKind::BindingRulingRequired,
                reason: format!("multiple viable classifications: {}", viable.join(", ")),
                authority: "customs classification authority".to_string(),
            }),
        )
    } else if !potentially_viable.is_empty() {
        (
            ClassificationOutcome::InsufficientFacts {
                missing: missing_facts(&request.facts, &request.candidates),
            },
            Some(Escalation {
                kind: EscalationKind::ProductFactsRequired,
                reason: "product facts do not decide any candidate".to_string(),
                authority: "product engineering / customs".to_string(),
            }),
        )
    } else {
        (
            ClassificationOutcome::NoCandidate,
            Some(Escalation {
                kind: EscalationKind::BindingRulingRequired,
                reason: "no candidate criteria are satisfied".to_string(),
                authority: "customs classification authority".to_string(),
            }),
        )
    };
    Ok(Classification {
        request_digest,
        outcome,
        evaluations,
        escalation,
        evidence,
    })
}

/// Accept an ambiguous classification with an explicit human approval.
/// The approval must bind to the entire classification and the chosen code;
/// accepting a code that is not among the viable candidates is refused even
/// with a token. Non-ambiguous outcomes need no approval.
pub fn accept_classification(
    classification: &Classification,
    code: &str,
    approval: Option<&faktor_approval::ApprovalToken>,
) -> Result<Classification, LogisticsError> {
    let mut accepted = classification.clone();
    match &classification.outcome {
        ClassificationOutcome::Classified { code: current } if current == code => Ok(accepted),
        ClassificationOutcome::Classified { code: current } => {
            Err(LogisticsError::ClassificationInconsistent(format!(
                "already classified as {current}, not {code}"
            )))
        }
        ClassificationOutcome::Ambiguous { codes } => {
            if classification.request_digest.len() != 64
                || !classification
                    .request_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(LogisticsError::ClassificationInconsistent(
                    "ambiguous classification is not bound to a product request".to_string(),
                ));
            }
            if !codes.iter().any(|candidate| candidate == code) {
                return Err(LogisticsError::ClassificationInconsistent(format!(
                    "code {code} is not one of the viable candidates {}",
                    codes.join(", ")
                )));
            }
            let digest = faktor_approval::content_digest(
                b"faktor.supply.hs-acceptance/v1",
                &(classification, code),
            )
            .map_err(ExactError::Approval)?;
            let check = faktor_approval::ApprovalCheck::new(
                faktor_approval::ApprovalKind::SupplyTariffClassification,
                "hs-classification",
                "current",
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
            accepted.outcome = ClassificationOutcome::Classified {
                code: code.to_string(),
            };
            accepted.escalation = None;
            Ok(accepted)
        }
        ClassificationOutcome::InsufficientFacts { missing } => {
            Err(LogisticsError::ClassificationNotFinal(format!(
                "product facts are incomplete: {}",
                missing.join(", ")
            )))
        }
        ClassificationOutcome::NoCandidate => Err(LogisticsError::ClassificationNotFinal(
            "no viable candidate exists; a ruling is required".to_string(),
        )),
    }
}

fn evaluate_criterion(
    criterion: &CandidateCriterion,
    facts: &ProductFacts,
) -> (&'static str, String) {
    match criterion {
        CandidateCriterion::CompositionAtLeast { material, min_ppm } => {
            match facts.share(material) {
                None => (
                    "unknown",
                    format!("composition for {material} is not known"),
                ),
                Some(share) if share >= *min_ppm => (
                    "satisfied",
                    format!("{material} share {share} ppm >= {min_ppm}"),
                ),
                Some(share) => (
                    "violated",
                    format!("{material} share {share} ppm < {min_ppm}"),
                ),
            }
        }
        CandidateCriterion::CompositionAtMost { material, max_ppm } => {
            match facts.share(material) {
                None => (
                    "unknown",
                    format!("composition for {material} is not known"),
                ),
                Some(share) if share <= *max_ppm => (
                    "satisfied",
                    format!("{material} share {share} ppm <= {max_ppm}"),
                ),
                Some(share) => (
                    "violated",
                    format!("{material} share {share} ppm > {max_ppm}"),
                ),
            }
        }
        CandidateCriterion::FunctionContains { text } => match &facts.function {
            None => ("unknown", "function is not known".to_string()),
            Some(function) if function.contains(text) => {
                ("satisfied", format!("function contains {text}"))
            }
            Some(_) => ("violated", format!("function does not contain {text}")),
        },
        CandidateCriterion::EssentialCharacterContains { text } => {
            match &facts.essential_character {
                None => ("unknown", "essential character is not known".to_string()),
                Some(character) if character.contains(text) => {
                    ("satisfied", format!("essential character contains {text}"))
                }
                Some(_) => (
                    "violated",
                    format!("essential character does not contain {text}"),
                ),
            }
        }
        CandidateCriterion::DescriptionContains { text } => {
            if facts.description.contains(text) {
                ("satisfied", format!("description contains {text}"))
            } else {
                ("violated", format!("description does not contain {text}"))
            }
        }
    }
}

fn missing_facts(facts: &ProductFacts, candidates: &[HsCandidate]) -> Vec<String> {
    let mut missing = Vec::new();
    for candidate in candidates {
        for criterion in &candidate.criteria {
            let (outcome, _) = evaluate_criterion(criterion, facts);
            if outcome == "unknown" {
                let label = criterion.label();
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
        }
    }
    missing.sort();
    missing
}

/// A customs document class.
#[derive(
    JsonSchema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CustomsDocumentKind {
    /// Commercial invoice.
    CommercialInvoice,
    /// Packing list.
    PackingList,
    /// Transport document (bill of lading, air waybill, CMR).
    TransportDocument,
    /// Certificate of origin.
    CertificateOfOrigin,
    /// Import declaration.
    ImportDeclaration,
    /// Preference / FTA evidence.
    PreferenceEvidence,
}

/// One evidenced customs document.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomsDocument {
    /// Kind.
    pub kind: CustomsDocumentKind,
    /// Revision-bound evidence.
    pub evidence: EvidenceRef,
}

/// A customs document set.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomsDocumentSet {
    /// Documents.
    #[serde(default)]
    pub documents: Vec<CustomsDocument>,
}

impl CustomsDocumentSet {
    /// Validate bounds and evidence.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        if self.documents.len() > 64 {
            return Err(LogisticsError::Bound {
                what: "customs documents",
                max: 64,
            });
        }
        for document in &self.documents {
            document.evidence.validate()?;
        }
        Ok(())
    }

    /// The required documents missing from the set.
    pub fn missing(&self, required: &[CustomsDocumentKind]) -> Vec<CustomsDocumentKind> {
        required
            .iter()
            .copied()
            .filter(|required| {
                !self
                    .documents
                    .iter()
                    .any(|document| document.kind == *required)
            })
            .collect()
    }
}

/// Documents required for an import, optionally including preference proof.
pub fn required_documents(preference_claimed: bool) -> Vec<CustomsDocumentKind> {
    let mut documents = vec![
        CustomsDocumentKind::CommercialInvoice,
        CustomsDocumentKind::PackingList,
        CustomsDocumentKind::TransportDocument,
        CustomsDocumentKind::ImportDeclaration,
    ];
    if preference_claimed {
        documents.push(CustomsDocumentKind::CertificateOfOrigin);
        documents.push(CustomsDocumentKind::PreferenceEvidence);
    }
    documents
}

/// Customs valuation basis.
#[derive(JsonSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValuationBasis {
    /// Free-on-board valuation: goods only.
    Fob,
    /// CIF valuation: goods + international freight + insurance.
    Cif,
}

/// Explicit valuation components.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomsValuation {
    /// Goods value.
    pub goods_value: Money,
    /// International freight.
    pub international_freight: Money,
    /// Insurance.
    pub insurance: Money,
    /// Basis.
    pub basis: ValuationBasis,
}

impl CustomsValuation {
    /// Validate that all components share one currency and are non-negative.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        let currency = self.goods_value.currency;
        for (value, what) in [
            (self.international_freight, "international freight"),
            (self.insurance, "insurance"),
        ] {
            if value.currency != currency {
                return Err(LogisticsError::CurrencyMismatch {
                    left: currency,
                    right: value.currency,
                });
            }
            if value.micros() < 0 {
                return Err(LogisticsError::Negative(what));
            }
        }
        if self.goods_value.micros() < 0 {
            return Err(LogisticsError::Negative("goods value"));
        }
        Ok(())
    }

    /// The customs value under the declared basis.
    pub fn value(&self) -> Result<Money, LogisticsError> {
        match self.basis {
            ValuationBasis::Fob => Ok(self.goods_value),
            ValuationBasis::Cif => self
                .goods_value
                .checked_add(self.international_freight)
                .and_then(|value| value.checked_add(self.insurance))
                .map_err(LogisticsError::Exact),
        }
    }
}

/// A tariff entry, supplied as an authoritative input.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TariffEntry {
    /// Destination country.
    pub country: String,
    /// HS code/tariff line.
    pub code: String,
    /// Most-favoured-nation duty rate.
    pub mfn_duty: Rate,
    /// Domestic tax rate (VAT/GST/...).
    pub tax: Rate,
    /// Preferential rate, only with an evidenced origin claim.
    pub preferential_duty: Option<Rate>,
    /// Tariff schedule revision.
    pub revision: Revision,
    /// Evidence.
    pub evidence: EvidenceRef,
}

impl TariffEntry {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        validate_text(&self.country, MAX_ID_BYTES, "tariff country")?;
        if !(6..=10).contains(&self.code.len())
            || !self.code.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(LogisticsError::Unavailable(
                "tariff code must be 6-10 digits".to_string(),
            ));
        }
        self.evidence.validate()?;
        Ok(())
    }
}

/// A tariff schedule.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TariffSchedule {
    /// Entries.
    pub entries: Vec<TariffEntry>,
    /// Schedule revision.
    pub revision: Revision,
}

impl TariffSchedule {
    /// Validate bounds and duplicate keys.
    pub fn validate(&self) -> Result<(), LogisticsError> {
        if self.entries.len() > MAX_TARIFF_ENTRIES {
            return Err(LogisticsError::Bound {
                what: "tariff entries",
                max: MAX_TARIFF_ENTRIES,
            });
        }
        let mut keys = std::collections::HashSet::new();
        for entry in &self.entries {
            entry.validate()?;
            if !keys.insert((entry.country.clone(), entry.code.clone())) {
                return Err(LogisticsError::Unavailable(format!(
                    "duplicate tariff entry {} / {}",
                    entry.country, entry.code
                )));
            }
        }
        Ok(())
    }

    /// Look up an entry.
    pub fn entry(&self, country: &str, code: &str) -> Option<&TariffEntry> {
        self.entries
            .iter()
            .find(|entry| entry.country == country && entry.code == code)
    }
}

/// An origin / preference claim.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginClaim {
    /// Origin country.
    pub country: String,
    /// Optional FTA preference claim.
    pub preference: Option<PreferenceClaim>,
}

/// An FTA preference claim with evidence validity.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceClaim {
    /// Agreement identifier.
    pub agreement: String,
    /// Evidence (certificate of origin, statement, ...).
    pub evidence: EvidenceRef,
    /// Valid from day.
    pub valid_from: Day,
    /// Valid until day.
    pub valid_until: Day,
    /// Revision.
    pub revision: Revision,
}

impl PreferenceClaim {
    fn valid_on(&self, day: Day) -> bool {
        !day.before(self.valid_from) && !self.valid_until.before(day)
    }
}

/// The preference decision.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PreferenceStatus {
    /// No preference was claimed.
    NotClaimed,
    /// Preference applied at the evidenced preferential rate.
    Applied {
        /// Agreement.
        agreement: String,
    },
    /// Preference was claimed but not evidenced/current; MFN applied.
    Unsubstantiated {
        /// Why.
        detail: String,
    },
    /// Preference is claimed but the tariff has no preferential rate.
    RateUnavailable {
        /// Why.
        detail: String,
    },
}

/// Duty/tax computation result.
#[derive(JsonSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DutyAssessment {
    /// Destination country.
    pub country: String,
    /// Code used.
    pub code: String,
    /// Tariff revision.
    pub tariff_revision: Revision,
    /// Customs value.
    pub customs_value: Money,
    /// Duty.
    pub duty: Money,
    /// Domestic tax (not adjusted for recoverability).
    pub tax: Money,
    /// Total duty + tax.
    pub total: Money,
    /// Preference decision.
    pub preference: PreferenceStatus,
    /// Evidence.
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
}

/// Compute duty and tax for a finalized classification. Any ambiguity refuses
/// computation; no rate is fabricated.
/// A classified outcome is a claim about the evaluation trail. Refuse a
/// document whose outcome does not name the one and only viable candidate
/// with non-empty evidence: a fabricated "classified" wrapper cannot smuggle
/// code `X` past duty arithmetic while its own trail says `Y` or says nothing.
fn verify_classified_trail(
    classification: &Classification,
    code: &str,
) -> Result<(), LogisticsError> {
    if classification.evidence.is_empty() {
        return Err(LogisticsError::ClassificationInconsistent(
            "a classified outcome carries no evidence references".to_string(),
        ));
    }
    let viable: Vec<&str> = classification
        .evaluations
        .iter()
        .filter(|evaluation| evaluation.viable)
        .map(|evaluation| evaluation.code.as_str())
        .collect();
    if viable.as_slice() != [code] {
        return Err(LogisticsError::ClassificationInconsistent(format!(
            "outcome names {code:?} but the viable candidate trail is {viable:?}"
        )));
    }
    if classification
        .evaluations
        .iter()
        .any(|evaluation| evaluation.code == code && !evaluation.viable)
    {
        return Err(LogisticsError::ClassificationInconsistent(format!(
            "candidate {code:?} is both viable and non-viable in the trail"
        )));
    }
    Ok(())
}

pub fn compute_duty(
    classification: &Classification,
    tariff: &TariffSchedule,
    country: &str,
    valuation: &CustomsValuation,
    origin: Option<&OriginClaim>,
    as_of: Day,
) -> Result<DutyAssessment, LogisticsError> {
    valuation.validate()?;
    let code = classification
        .code()
        .ok_or_else(|| {
            LogisticsError::ClassificationNotFinal(format!("{:?}", classification.outcome))
        })?
        .to_string();
    verify_classified_trail(classification, &code)?;
    tariff.validate()?;
    validate_text(country, MAX_ID_BYTES, "destination country")?;
    let entry = tariff
        .entry(country, &code)
        .ok_or_else(|| LogisticsError::MissingTariffEntry {
            country: country.to_string(),
            code: code.clone(),
        })?;
    let customs_value = valuation.value()?;
    if customs_value.micros() < 0 {
        return Err(LogisticsError::Negative("customs value"));
    }
    let mut evidence = vec![entry.evidence.clone()];
    for reference in &classification.evidence {
        if !evidence.contains(reference) {
            evidence.push(reference.clone());
        }
    }
    let (duty_rate, preference) = match origin.and_then(|origin| origin.preference.as_ref()) {
        None => (entry.mfn_duty, PreferenceStatus::NotClaimed),
        Some(claim) => {
            evidence.push(claim.evidence.clone());
            let origin_country = origin
                .map(|origin| origin.country.as_str())
                .unwrap_or_default();
            if origin_country.is_empty() {
                (
                    entry.mfn_duty,
                    PreferenceStatus::Unsubstantiated {
                        detail: "preference claimed without an origin country".to_string(),
                    },
                )
            } else if !claim.valid_on(as_of) {
                (
                    entry.mfn_duty,
                    PreferenceStatus::Unsubstantiated {
                        detail: format!(
                            "preference validity {}..={} does not cover day {}",
                            claim.valid_from.0, claim.valid_until.0, as_of.0
                        ),
                    },
                )
            } else if let Some(preferential) = entry.preferential_duty {
                (
                    preferential,
                    PreferenceStatus::Applied {
                        agreement: claim.agreement.clone(),
                    },
                )
            } else {
                (
                    entry.mfn_duty,
                    PreferenceStatus::RateUnavailable {
                        detail: "tariff has no preferential rate for this code".to_string(),
                    },
                )
            }
        }
    };
    let duty = customs_value.scale(duty_rate)?;
    let taxable = customs_value.checked_add(duty)?;
    let tax = taxable.scale(entry.tax)?;
    let total = duty.checked_add(tax)?;
    Ok(DutyAssessment {
        country: country.to_string(),
        code,
        tariff_revision: tariff.revision,
        customs_value,
        duty,
        tax,
        total,
        preference,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_classification_acceptance_requires_a_human_approval() {
        let classification = Classification {
            request_digest: "b".repeat(64),
            outcome: ClassificationOutcome::Ambiguous {
                codes: vec!["850110".to_string(), "850120".to_string()],
            },
            evaluations: vec![],
            escalation: Some(Escalation {
                kind: EscalationKind::BindingRulingRequired,
                reason: "two viable codes".to_string(),
                authority: "customs classification authority".to_string(),
            }),
            evidence: vec![],
        };
        let error = accept_classification(&classification, "850110", None).unwrap_err();
        assert!(matches!(
            error,
            LogisticsError::Exact(ExactError::Approval(
                faktor_approval::ApprovalError::ApprovalRequired { .. }
            ))
        ));
        let digest = faktor_approval::content_digest(
            b"faktor.supply.hs-acceptance/v1",
            &(&classification, "850110"),
        )
        .unwrap();
        let token = faktor_approval::ApprovalToken::test_token(
            faktor_approval::ApprovalKind::SupplyTariffClassification,
            "hs-classification",
            "current",
            digest,
        );
        let accepted = accept_classification(&classification, "850110", Some(&token)).unwrap();
        assert_eq!(accepted.code(), Some("850110"));
        assert!(accepted.escalation.is_none());

        let mut other_product = classification.clone();
        other_product.request_digest = "c".repeat(64);
        let error = accept_classification(&other_product, "850110", Some(&token)).unwrap_err();
        assert!(
            matches!(
                error,
                LogisticsError::Exact(ExactError::Approval(
                    faktor_approval::ApprovalError::Stale { .. }
                ))
            ),
            "an approval for one product request must not authorize another: {error}"
        );

        let mut unbound = classification;
        unbound.request_digest.clear();
        assert!(accept_classification(&unbound, "850110", Some(&token)).is_err());
    }

    use faktor_supply_core::{EvidenceRef, Provenance, Revision, SourceKind};

    const CUR: Currency = Currency::USD;
    const DAY: i64 = 20_000;

    fn money(amount: &str) -> Money {
        Money::parse(amount, CUR).unwrap()
    }

    fn provenance(id: &str) -> Provenance {
        Provenance::new(
            SourceKind::PublicTariff,
            EvidenceRef::new(id, Revision(1), None).unwrap(),
            Day(DAY - 1),
            "fixture",
            vec!["tariff revision 1".to_string()],
            "a newer tariff revision",
        )
        .unwrap()
    }

    fn term<T>(value: T, id: &str) -> Term<T> {
        Term::evidence(value, provenance(id))
    }

    fn estimate(leg: CostLeg, amount: &str) -> LegEstimate {
        LegEstimate {
            leg,
            amount: term(money(amount), "leg"),
        }
    }

    #[test]
    fn incoterm_legallocation_is_explicit_and_total() {
        let estimates = CostLeg::ALL
            .iter()
            .map(|leg| estimate(*leg, "10.00"))
            .collect::<Vec<_>>();
        let fob = allocate_legs(Incoterm::Fob, CUR, &estimates).unwrap();
        assert!(fob.seller_legs.contains(&CostLeg::ExportClearance));
        assert!(fob.seller_legs.contains(&CostLeg::LoadingAtOrigin));
        assert!(fob.buyer_legs.contains(&CostLeg::MainCarriage));
        assert!(fob.buyer_legs.contains(&CostLeg::DutyAndTax));
        assert!(fob.missing_legs.is_empty());
        // buyer legs: main, insurance, import, duty, destination inland,
        // unloading = 6 x 10.
        assert_eq!(
            fob.buyer_total.as_known().unwrap().to_string(),
            "60.000000 USD"
        );
        assert_eq!(fob.risk_transfer, RiskTransferPoint::OnBoard);

        let exw = allocate_legs(Incoterm::Exw, CUR, &estimates).unwrap();
        assert!(exw.buyer_legs.len() > fob.buyer_legs.len());
        assert!(exw.seller_legs.contains(&CostLeg::ExportPackaging));
        assert!(!exw.seller_legs.contains(&CostLeg::ExportClearance));
        assert_eq!(exw.risk_transfer, RiskTransferPoint::SellerPremises);

        let ddp = allocate_legs(Incoterm::Ddp, CUR, &estimates).unwrap();
        assert!(ddp.seller_legs.contains(&CostLeg::DutyAndTax));
        assert_eq!(
            ddp.buyer_total.as_known().unwrap().to_string(),
            "20.000000 USD"
        );
        assert_eq!(ddp.risk_transfer, RiskTransferPoint::DestinationCleared);
    }

    #[test]
    fn missing_buyer_leg_estimate_is_unknown() {
        let estimates = vec![estimate(CostLeg::MainCarriage, "100.00")];
        let allocation = allocate_legs(Incoterm::Fob, CUR, &estimates).unwrap();
        assert!(!allocation.buyer_total.is_known());
        assert!(allocation.missing_legs.contains(&CostLeg::DutyAndTax));
        assert!(format!("{:?}", allocation.buyer_total).contains("missing buyer leg"));
    }

    #[test]
    fn maritime_terms_refuse_air() {
        assert!(validate_term_for_mode(Incoterm::Fob, TransportMode::OceanFcl).is_ok());
        assert!(matches!(
            validate_term_for_mode(Incoterm::Fob, TransportMode::Air),
            Err(LogisticsError::ModeIncompatible { .. })
        ));
        assert!(validate_term_for_mode(Incoterm::Cip, TransportMode::Air).is_ok());
    }

    #[test]
    fn chargeable_weight_uses_max_of_actual_and_volumetric() {
        // 600 x 400 x 300 mm = 72,000 cm3; at 6000 cm3/kg -> 12 kg = 12,000 g.
        let package = Package {
            length_mm: 600,
            width_mm: 400,
            height_mm: 300,
            actual_weight_g: 8_000,
            quantity: 1,
        };
        let weight = chargeable_weight(&[package], 6_000).unwrap();
        assert_eq!(weight.volumetric_g, 12_000);
        assert_eq!(weight.chargeable_g, 12_000);
        // Actual dominates.
        let heavy = Package {
            actual_weight_g: 20_000,
            ..package
        };
        assert_eq!(
            chargeable_weight(&[heavy], 6_000).unwrap().chargeable_g,
            20_000
        );
        // Quantity multiplies.
        let many = Package {
            quantity: 3,
            ..package
        };
        let weight = chargeable_weight(&[many], 6_000).unwrap();
        assert_eq!(weight.chargeable_g, 36_000);
        assert_eq!(weight.packages, 3);
    }

    #[test]
    fn zero_dimensions_and_divisor_are_refused() {
        let package = Package {
            length_mm: 0,
            width_mm: 1,
            height_mm: 1,
            actual_weight_g: 1,
            quantity: 1,
        };
        assert!(chargeable_weight(&[package], 6_000).is_err());
        assert!(chargeable_weight(&[], 0).is_err());
    }

    #[test]
    fn consolidation_groups_lanes_and_is_deterministic() {
        let shipments = vec![
            (
                "s2".to_string(),
                "CN".to_string(),
                "US".to_string(),
                TransportMode::Air,
                100,
            ),
            (
                "s1".to_string(),
                "CN".to_string(),
                "US".to_string(),
                TransportMode::Air,
                200,
            ),
            (
                "s3".to_string(),
                "DE".to_string(),
                "US".to_string(),
                TransportMode::Air,
                50,
            ),
        ];
        let groups = consolidate(&shipments).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].shipments, vec!["s1", "s2"]);
        assert_eq!(groups[0].chargeable_g, 300);
    }

    #[test]
    fn mode_selection_is_cheapest_feasible() {
        let dist = Distribution {
            p10: 2,
            p50: 4,
            p90: 6,
            p99: 8,
            sample_size: 50,
        };
        let options = vec![
            ModeOption {
                mode: TransportMode::Air,
                transit_days: term(dist, "air-transit"),
                fixed_cost: term(money("100.00"), "air-fixed"),
                cost_per_kg: term(money("5.00"), "air-kg"),
                minimum_charge: term(money("100.00"), "air-min"),
                max_weight_g: 1_000_000,
            },
            ModeOption {
                mode: TransportMode::OceanLcl,
                transit_days: term(
                    Distribution {
                        p10: 20,
                        p50: 28,
                        p90: 35,
                        p99: 45,
                        sample_size: 50,
                    },
                    "sea-transit",
                ),
                fixed_cost: term(money("50.00"), "sea-fixed"),
                cost_per_kg: term(money("1.00"), "sea-kg"),
                minimum_charge: term(money("60.00"), "sea-min"),
                max_weight_g: 10_000_000,
            },
        ];
        let selected = select_mode(&options, 10_000, 40).unwrap();
        let selected = selected.as_known().unwrap();
        assert_eq!(selected.mode, TransportMode::OceanLcl);
        assert_eq!(selected.cost.to_string(), "60.000000 USD");
        let urgent = select_mode(&options, 10_000, 7).unwrap();
        assert_eq!(urgent.as_known().unwrap().mode, TransportMode::Air);
        let impossible = select_mode(&options, 10_000, 2).unwrap();
        assert!(!impossible.is_known());
        assert_eq!(
            impossible.unavailable_reason().unwrap().reason,
            UnavailableReason::NoCandidate
        );
    }

    #[test]
    fn demurrage_respects_free_days_and_unknown_dwell() {
        let terms = DemurrageTerms {
            free_days: 5,
            daily_rate: money("100.00"),
            storage_per_day: Term::not_applicable("storage included"),
        };
        let dwell = term(
            Distribution {
                p10: 3,
                p50: 7,
                p90: 10,
                p99: 15,
                sample_size: 20,
            },
            "dwell",
        );
        assert_eq!(
            terms
                .expected(&dwell, Quantile::P50)
                .as_known()
                .unwrap()
                .to_string(),
            "200.000000 USD"
        );
        assert_eq!(
            terms
                .expected(&dwell, Quantile::P10)
                .as_known()
                .unwrap()
                .to_string(),
            "0.000000 USD"
        );
        let unknown: Term<Distribution> =
            Term::unknown(UnavailableReason::MissingEvidence, "no port dwell history");
        assert!(!terms.expected(&unknown, Quantile::P90).is_known());
    }

    #[test]
    fn insurance_uses_coverage_and_minimum() {
        let terms = InsuranceTerms {
            rate: Rate::new(5_000).unwrap(),
            minimum_premium: money("25.00"),
            coverage_ppm: 1_100_000,
        };
        assert_eq!(
            terms.premium(money("10000.00")).unwrap().to_string(),
            "55.000000 USD"
        );
        assert_eq!(
            terms.premium(money("100.00")).unwrap().to_string(),
            "25.000000 USD"
        );
    }

    fn facts() -> ProductFacts {
        ProductFacts {
            description: "Aluminium electrolytic capacitor".to_string(),
            composition: Some(vec![
                CompositionShare {
                    material: "aluminium".to_string(),
                    share_ppm: 600_000,
                },
                CompositionShare {
                    material: "electrolyte".to_string(),
                    share_ppm: 300_000,
                },
            ]),
            function: Some("energy storage and filtering".to_string()),
            essential_character: Some("aluminium electrolytic element".to_string()),
        }
    }

    fn candidate(code: &str, criteria: Vec<CandidateCriterion>) -> HsCandidate {
        HsCandidate {
            code: code.to_string(),
            description: "candidate".to_string(),
            criteria,
            gir: vec![InterpretiveRule {
                rule: GirRule::G1,
                note: "terms of the heading".to_string(),
                provenance: provenance("gir"),
            }],
            evidence: EvidenceRef::new(&format!("hs-{code}"), Revision(1), None).unwrap(),
            revision: Revision(1),
        }
    }

    fn classification_request() -> ClassificationRequest {
        ClassificationRequest {
            facts: facts(),
            candidates: vec![
                candidate(
                    "853222",
                    vec![
                        CandidateCriterion::CompositionAtLeast {
                            material: "aluminium".to_string(),
                            min_ppm: 500_000,
                        },
                        CandidateCriterion::FunctionContains {
                            text: "energy storage".to_string(),
                        },
                    ],
                ),
                candidate(
                    "853225",
                    vec![CandidateCriterion::CompositionAtLeast {
                        material: "ceramic".to_string(),
                        min_ppm: 500_000,
                    }],
                ),
            ],
        }
    }

    #[test]
    fn unique_viable_candidate_is_classified() {
        let result = classify(&classification_request()).unwrap();
        assert_eq!(result.code(), Some("853222"));
        assert!(result.escalation.is_none());
        assert_eq!(result.evidence.len(), 3);
    }

    #[test]
    fn multiple_viable_candidates_escalate_ambiguous() {
        let mut request = classification_request();
        request.candidates[1] = candidate(
            "853225",
            vec![CandidateCriterion::CompositionAtLeast {
                material: "aluminium".to_string(),
                min_ppm: 100_000,
            }],
        );
        let result = classify(&request).unwrap();
        assert!(matches!(
            result.outcome,
            ClassificationOutcome::Ambiguous { .. }
        ));
        assert_eq!(
            result.escalation.as_ref().unwrap().kind,
            EscalationKind::BindingRulingRequired
        );
        assert!(result.code().is_none());
    }

    #[test]
    fn a_viable_candidate_plus_an_undecidable_one_is_not_classified() {
        let mut request = classification_request();
        request.facts.essential_character = None;
        request.candidates.push(candidate(
            "853230",
            vec![CandidateCriterion::EssentialCharacterContains {
                text: "electrolytic".to_string(),
            }],
        ));
        let result = classify(&request).unwrap();
        assert!(matches!(
            result.outcome,
            ClassificationOutcome::InsufficientFacts { .. }
        ));
        assert!(result.escalation.is_some());
    }

    #[test]
    fn missing_composition_is_insufficient_and_escalated() {
        let mut request = classification_request();
        request.facts.composition = None;
        let result = classify(&request).unwrap();
        assert!(matches!(
            result.outcome,
            ClassificationOutcome::InsufficientFacts { .. }
        ));
        assert_eq!(
            result.escalation.as_ref().unwrap().kind,
            EscalationKind::ProductFactsRequired
        );
    }

    #[test]
    fn no_viable_candidate_is_not_guessed() {
        let mut request = classification_request();
        request.facts.composition = Some(vec![CompositionShare {
            material: "titanium".to_string(),
            share_ppm: 1_000_000,
        }]);
        let result = classify(&request).unwrap();
        assert!(matches!(result.outcome, ClassificationOutcome::NoCandidate));
        assert!(result.escalation.is_some());
    }

    #[test]
    fn candidate_without_gir_provenance_is_refused() {
        let mut request = classification_request();
        request.candidates[0].gir.clear();
        assert!(classify(&request).is_err());
    }

    #[test]
    fn invalid_composition_sum_is_refused() {
        let mut request = classification_request();
        request.facts.composition = Some(vec![
            CompositionShare {
                material: "a".to_string(),
                share_ppm: 700_000,
            },
            CompositionShare {
                material: "b".to_string(),
                share_ppm: 700_000,
            },
        ]);
        assert!(classify(&request).is_err());
    }

    fn tariff() -> TariffSchedule {
        TariffSchedule {
            entries: vec![TariffEntry {
                country: "US".to_string(),
                code: "853222".to_string(),
                mfn_duty: Rate::new(25_000).unwrap(),
                tax: Rate::new(0).unwrap(),
                preferential_duty: Some(Rate::new(0).unwrap()),
                revision: Revision(2),
                evidence: EvidenceRef::new("tariff", Revision(2), None).unwrap(),
            }],
            revision: Revision(2),
        }
    }

    #[test]
    fn json_fixture_classifies_a_capacitor() {
        let request: ClassificationRequest =
            serde_json::from_str(include_str!("../tests/fixtures/classification.json")).unwrap();
        let result = classify(&request).unwrap();
        assert_eq!(result.code(), Some("853222"));
        assert!(result.escalation.is_none());
    }

    #[test]
    fn duty_requires_final_classification() {
        let mut request = classification_request();
        request.facts.composition = None;
        let classification = classify(&request).unwrap();
        let valuation = CustomsValuation {
            goods_value: money("1000.00"),
            international_freight: money("100.00"),
            insurance: money("10.00"),
            basis: ValuationBasis::Cif,
        };
        assert!(matches!(
            compute_duty(&classification, &tariff(), "US", &valuation, None, Day(DAY)),
            Err(LogisticsError::ClassificationNotFinal(_))
        ));
    }

    #[test]
    fn forged_classified_outcome_cannot_borrow_another_classification_trail() {
        let mut forged = classify(&classification_request()).unwrap();
        let genuine_code = forged.code().unwrap().to_string();
        let fabricated = if genuine_code == "999999" {
            "853222"
        } else {
            "999999"
        };
        forged.outcome = ClassificationOutcome::Classified {
            code: fabricated.to_string(),
        };
        let valuation = CustomsValuation {
            goods_value: money("1000.00"),
            international_freight: money("0.00"),
            insurance: money("0.00"),
            basis: ValuationBasis::Fob,
        };
        assert!(matches!(
            compute_duty(&forged, &tariff(), "US", &valuation, None, Day(DAY)),
            Err(LogisticsError::ClassificationInconsistent(_))
        ));
        let mut no_evidence = classify(&classification_request()).unwrap();
        no_evidence.evidence.clear();
        assert!(matches!(
            compute_duty(&no_evidence, &tariff(), "US", &valuation, None, Day(DAY)),
            Err(LogisticsError::ClassificationInconsistent(_))
        ));
    }

    #[test]
    fn duty_and_tax_are_exact_with_preference() {
        let classification = classify(&classification_request()).unwrap();
        let valuation = CustomsValuation {
            goods_value: money("1000.00"),
            international_freight: money("100.00"),
            insurance: money("10.00"),
            basis: ValuationBasis::Cif,
        };
        let mfn =
            compute_duty(&classification, &tariff(), "US", &valuation, None, Day(DAY)).unwrap();
        assert_eq!(mfn.customs_value.to_string(), "1110.000000 USD");
        assert_eq!(mfn.duty.to_string(), "27.750000 USD");
        assert_eq!(mfn.tax.micros(), 0);
        assert!(matches!(mfn.preference, PreferenceStatus::NotClaimed));
        assert!(
            mfn.evidence
                .iter()
                .any(|reference| reference.id == "hs-853222"),
            "duty evidence must carry the classification trail: {:?}",
            mfn.evidence
        );

        let preference = OriginClaim {
            country: "MX".to_string(),
            preference: Some(PreferenceClaim {
                agreement: "USMCA".to_string(),
                evidence: EvidenceRef::new("coo", Revision(1), None).unwrap(),
                valid_from: Day(DAY - 10),
                valid_until: Day(DAY + 10),
                revision: Revision(1),
            }),
        };
        let preferred = compute_duty(
            &classification,
            &tariff(),
            "US",
            &valuation,
            Some(&preference),
            Day(DAY),
        )
        .unwrap();
        assert_eq!(preferred.duty.micros(), 0);
        assert!(matches!(
            preferred.preference,
            PreferenceStatus::Applied { .. }
        ));
    }

    #[test]
    fn expired_preference_falls_back_to_mfn_and_says_so() {
        let classification = classify(&classification_request()).unwrap();
        let valuation = CustomsValuation {
            goods_value: money("1000.00"),
            international_freight: money("0.00"),
            insurance: money("0.00"),
            basis: ValuationBasis::Fob,
        };
        let preference = OriginClaim {
            country: "MX".to_string(),
            preference: Some(PreferenceClaim {
                agreement: "USMCA".to_string(),
                evidence: EvidenceRef::new("coo", Revision(1), None).unwrap(),
                valid_from: Day(DAY - 100),
                valid_until: Day(DAY - 10),
                revision: Revision(1),
            }),
        };
        let result = compute_duty(
            &classification,
            &tariff(),
            "US",
            &valuation,
            Some(&preference),
            Day(DAY),
        )
        .unwrap();
        assert_eq!(result.duty.to_string(), "25.000000 USD");
        assert!(matches!(
            result.preference,
            PreferenceStatus::Unsubstantiated { .. }
        ));
    }

    #[test]
    fn missing_tariff_entry_refuses() {
        let classification = classify(&classification_request()).unwrap();
        let valuation = CustomsValuation {
            goods_value: money("1000.00"),
            international_freight: money("0.00"),
            insurance: money("0.00"),
            basis: ValuationBasis::Fob,
        };
        assert!(matches!(
            compute_duty(&classification, &tariff(), "DE", &valuation, None, Day(DAY)),
            Err(LogisticsError::MissingTariffEntry { .. })
        ));
    }

    #[test]
    fn customs_documents_are_typed_and_missing_ones_are_named() {
        let set = CustomsDocumentSet {
            documents: vec![
                CustomsDocument {
                    kind: CustomsDocumentKind::CommercialInvoice,
                    evidence: EvidenceRef::new("invoice", Revision(1), None).unwrap(),
                },
                CustomsDocument {
                    kind: CustomsDocumentKind::PackingList,
                    evidence: EvidenceRef::new("packing", Revision(1), None).unwrap(),
                },
            ],
        };
        set.validate().unwrap();
        let missing = set.missing(&required_documents(true));
        assert!(missing.contains(&CustomsDocumentKind::TransportDocument));
        assert!(missing.contains(&CustomsDocumentKind::CertificateOfOrigin));
        assert!(!missing.contains(&CustomsDocumentKind::CommercialInvoice));
    }

    #[test]
    fn dangerous_goods_requires_valid_un_number_and_declaration() {
        let declaration = DangerousGoods {
            un_number: "UN3480".to_string(),
            hazard_class: "9".to_string(),
            packing_group: None,
            declaration: EvidenceRef::new("dg", Revision(1), None).unwrap(),
        };
        assert!(declaration.validate().is_ok());
        let mut bad = declaration.clone();
        bad.un_number = "348".to_string();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn handling_rounds_up_to_whole_kg() {
        let charge = HandlingCharge {
            per_shipment: money("25.00"),
            per_kg: money("2.50"),
        };
        assert_eq!(charge.cost(1_001).unwrap().to_string(), "30.000000 USD");
        assert_eq!(charge.cost(1).unwrap().to_string(), "27.500000 USD");
    }
}
