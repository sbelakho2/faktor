use std::collections::BTreeMap;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ValueState, WarningSeverity,
};
use engineering_units::{bisection, Dimension, Interval, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_finite, require_positive, unavailable_result, value_in, warning, AssumptionRecord,
    CrossCheckEvidence, ModelContext, ModelError, ModelReport, ModelResult, ModelStatus, Refusal,
    RefusalCode, MODEL_VERSION,
};

pub const MOSFET_THERMAL_MODEL: &str = "power-electronics/mosfet-conduction-thermal";
const MODEL_EXPRESSION: &str =
    "P = I_rms^2 R_ds(T_j); T_j = T_a + P (R_th,jc + R_th,cs + R_th,sa); R_ds(T) = R_ref (1 + alpha (T - T_ref))";

/// A nominal value with an explicit uncertainty. Interval bounds are in the
/// nominal value's own unit before conversion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub nominal: Quantity,
    pub uncertainty: Uncertainty,
}

impl Estimate {
    pub fn new(nominal: Quantity, uncertainty: Uncertainty) -> ModelResult<Self> {
        require_finite("estimate.nominal", nominal.value())?;
        Ok(Self {
            nominal,
            uncertainty,
        })
    }

    /// Worst-case interval converted to `unit` (which must share the
    /// dimension). A normal distribution contributes its documented 3-sigma
    /// envelope, not a hard bound.
    pub fn interval_in(&self, unit: &str) -> ModelResult<Interval> {
        let target = UnitRegistry::si().require(unit)?;
        let nominal_si = self.nominal.to_unit(target.clone())?.value();
        let Some(bounds) = self.uncertainty.interval() else {
            return Ok(Interval::point(nominal_si)?);
        };
        let lower = Quantity::new(bounds.lower(), self.nominal.unit().clone())?
            .to_unit(target.clone())?
            .value();
        let upper = Quantity::new(bounds.upper(), self.nominal.unit().clone())?
            .to_unit(target)?
            .value();
        Ok(Interval::new(lower.min(upper), lower.max(upper))?)
    }

    pub fn nominal_in(&self, unit: &str) -> ModelResult<f64> {
        value_in(&self.nominal, unit)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MosfetThermalInput {
    pub drain_current_rms: Estimate,
    pub rds_on_reference: Estimate,
    pub temp_coefficient: Estimate,
    pub reference_temperature: Quantity,
    pub ambient_temperature: Estimate,
    pub r_theta_jc: Estimate,
    /// The interface resistance is a required input. `None` means the design
    /// has not established it; the model refuses rather than using zero.
    pub r_theta_cs: Option<Estimate>,
    pub r_theta_sa: Estimate,
    pub max_junction_temperature: ValueState<Quantity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MosfetThermalResult {
    pub conduction_loss: Quantity,
    pub junction_temperature: Quantity,
    pub rds_on_hot: Quantity,
    pub thermal_margin: ValueState<Quantity>,
    pub loss_interval: Option<Interval>,
    pub junction_interval_kelvin: Option<Interval>,
    pub stability_denominator: f64,
}

fn assumption(id: &str, statement: &str, citation: &str) -> AssumptionRecord {
    AssumptionRecord::new(
        id,
        statement,
        Provenance::new(ProvenanceKind::EngineeringPractice, citation)
            .expect("static non-empty provenance"),
    )
}

fn assumptions() -> Vec<AssumptionRecord> {
    vec![
        assumption(
            "asm-conduction-only",
            "Only conduction loss is modeled; switching, gate-drive, diode, avalanche and leakage losses are separate.",
            "Power-electronics knowledge pack: loss components.",
        ),
        assumption(
            "asm-linear-rds",
            "Rds(on) varies linearly with temperature inside the datasheet-derived interval.",
            "Manufacturer datasheet Rds(on) versus temperature curve.",
        ),
        assumption(
            "asm-constant-rth",
            "The supplied junction-case, case-sink and sink-ambient thermal resistances are constant for the actual assembly.",
            "JEDEC JESD51 thermal measurement conditions.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "P = I_rms^2 R_ds",
            Dimension::POWER,
            Dimension::CURRENT
                .checked_powi(2)?
                .checked_mul(Dimension::RESISTANCE)?,
        )?,
        DimensionCheck::new(
            "T_j = T_a + P R_th",
            Dimension::TEMPERATURE,
            Dimension::TEMPERATURE
                .checked_add(Dimension::POWER.checked_mul(Dimension::THERMAL_RESISTANCE)?)?,
        )?,
        DimensionCheck::new(
            "R_ds(T) = R_ref (1 + alpha (T - T_ref))",
            Dimension::RESISTANCE,
            Dimension::RESISTANCE.checked_mul(Dimension::SCALAR)?,
        )?,
        DimensionCheck::new(
            "alpha has inverse-temperature dimension",
            Dimension::INVERSE_TEMPERATURE,
            Dimension::INVERSE_TEMPERATURE,
        )?,
        DimensionCheck::new(
            "1 - R_th I_rms^2 R_ref alpha",
            Dimension::SCALAR,
            Dimension::SCALAR.checked_sub(
                Dimension::THERMAL_RESISTANCE
                    .checked_mul(Dimension::CURRENT.checked_powi(2)?)?
                    .checked_mul(Dimension::RESISTANCE)?
                    .checked_mul(Dimension::INVERSE_TEMPERATURE)?,
            )?,
        )?,
    ])
}

fn kelvin_value(quantity: &Quantity) -> ModelResult<f64> {
    require_dimension("temperature", quantity, Dimension::TEMPERATURE)?;
    value_in(quantity, "K")
}

fn temp_k(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("K")?)?)
}

fn ohm(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("ohm")?)?)
}

fn watt(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("W")?)?)
}

fn celsius_from_k(kelvin: f64) -> ModelResult<Quantity> {
    Ok(temp_k(kelvin)?.to_unit(UnitRegistry::si().require("degC")?)?)
}

fn refusal_report(
    input: &MosfetThermalInput,
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<MosfetThermalResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    bcs.insert(
        "drain_current_rms".to_string(),
        boundary_value(&input.drain_current_rms.nominal, context)?,
    );
    bcs.insert(
        "rds_on_reference".to_string(),
        boundary_value(&input.rds_on_reference.nominal, context)?,
    );
    bcs.insert(
        "ambient_temperature".to_string(),
        boundary_value(&input.ambient_temperature.nominal, context)?,
    );
    bcs.insert(
        "reference_temperature".to_string(),
        boundary_value(&input.reference_temperature, context)?,
    );
    let results = vec![
        unavailable_result(
            "conduction_loss",
            reason.clone(),
            vec!["complete thermal path and stable operating point".to_string()],
        )?,
        unavailable_result(
            "junction_temperature",
            reason.clone(),
            vec!["complete thermal path and stable operating point".to_string()],
        )?,
    ];
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let receipt = build_receipt(
        MOSFET_THERMAL_MODEL,
        input,
        MODEL_EXPRESSION,
        &input,
        context,
        bcs,
        Convergence::iterative(1, 0.0, 1.0, false),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Refused,
        value: ValueState::unavailable(reason, vec!["valid thermal analysis".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

pub fn mosfet_conduction_thermal(
    input: &MosfetThermalInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<MosfetThermalResult>> {
    let checks = checks()?;
    require_dimension(
        "reference_temperature",
        &input.reference_temperature,
        Dimension::TEMPERATURE,
    )?;
    let t_ref = kelvin_value(&input.reference_temperature)?;
    require_positive("reference_temperature", t_ref)?;

    let current = input.drain_current_rms.interval_in("A")?;
    let rds = input.rds_on_reference.interval_in("ohm")?;
    let alpha = input.temp_coefficient.interval_in("1/K")?;
    let ta = input.ambient_temperature.interval_in("K")?;
    let r_jc = input.r_theta_jc.interval_in("K/W")?;
    let r_sa = input.r_theta_sa.interval_in("K/W")?;
    let Some(r_cs_est) = &input.r_theta_cs else {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::MissingInterfaceResistance,
                "case-to-sink interface resistance is not established; a missing interface is unknown, not zero",
                vec![
                    "revision-bound thermal interface material data at the assembly pressure".to_string(),
                ],
            ),
            checks,
        );
    };
    let r_cs = r_cs_est.interval_in("K/W")?;

    for (name, interval) in [
        ("drain_current_rms", current),
        ("rds_on_reference", rds),
        ("r_theta_jc", r_jc),
        ("r_theta_cs", r_cs),
        ("r_theta_sa", r_sa),
    ] {
        if interval.lower() <= 0.0 {
            return Err(ModelError::InvalidInput {
                field: name,
                reason: "interval must be strictly positive".to_string(),
            });
        }
    }
    for (name, value) in [
        (
            "drain_current_rms",
            input.drain_current_rms.nominal_in("A")?,
        ),
        (
            "rds_on_reference",
            input.rds_on_reference.nominal_in("ohm")?,
        ),
        (
            "temp_coefficient",
            input.temp_coefficient.nominal_in("1/K")?,
        ),
        (
            "ambient_temperature",
            input.ambient_temperature.nominal_in("K")?,
        ),
        ("r_theta_jc", input.r_theta_jc.nominal_in("K/W")?),
        ("r_theta_cs", r_cs_est.nominal_in("K/W")?),
        ("r_theta_sa", input.r_theta_sa.nominal_in("K/W")?),
    ] {
        require_finite(name, value)?;
    }
    let current_nom = input.drain_current_rms.nominal_in("A")?;
    let rds_nom = input.rds_on_reference.nominal_in("ohm")?;
    let alpha_nom = input.temp_coefficient.nominal_in("1/K")?;
    let ta_nom = input.ambient_temperature.nominal_in("K")?;
    let rth_nom = input.r_theta_jc.nominal_in("K/W")?
        + r_cs_est.nominal_in("K/W")?
        + input.r_theta_sa.nominal_in("K/W")?;
    require_positive("r_theta_total", rth_nom)?;

    let a = current_nom * current_nom * rds_nom;
    let b = a * alpha_nom;
    let denominator = 1.0 - rth_nom * b;
    if denominator <= 0.0 {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::ThermalRunaway,
                format!(
                    "steady-state stability denominator 1 - R_th I^2 R_ref alpha = {denominator} is not positive: thermal runaway"
                ),
                vec!["lower Rds(on) loss, lower thermal resistance or lower current".to_string()],
            ),
            checks,
        );
    }
    let tj = t_ref + (ta_nom - t_ref + rth_nom * a) / denominator;
    let loss = a + b * (tj - t_ref);
    let rds_hot = rds_nom * (1.0 + alpha_nom * (tj - t_ref));
    let residual = (tj - (ta_nom + loss * rth_nom)).abs();

    let rth_interval = r_jc.add(r_cs).add(r_sa);
    let a_interval = current.mul(current).mul(rds);
    let b_interval = a_interval.mul(alpha);
    let denominator_interval = Interval::point(1.0)?.sub(rth_interval.mul(b_interval));
    if denominator_interval.lower() <= 0.0 {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::ThermalRunaway,
                "the declared uncertainty envelope reaches or crosses the thermal-runaway boundary; no bounded junction temperature exists",
                vec!["narrow the thermal/electrical uncertainty or reduce loss".to_string()],
            ),
            checks,
        );
    }
    let numerator = ta
        .sub(Interval::point(t_ref)?)
        .add(rth_interval.mul(a_interval));
    let tj_interval = Interval::point(t_ref)?.add(numerator.div(denominator_interval)?);
    let loss_interval = a_interval.add(b_interval.mul(tj_interval.sub(Interval::point(t_ref)?)));

    let thermal_margin = match &input.max_junction_temperature {
        ValueState::Known(limit) => {
            let limit_k = kelvin_value(limit)?;
            let margin = limit_k - tj;
            let quantity = Quantity::new(margin, UnitRegistry::si().require("degC_delta")?)?;
            ValueState::known(quantity)
        }
        ValueState::Unavailable(reason) => ValueState::Unavailable(reason.clone()),
    };

    let mut warnings = Vec::new();
    if input.rds_on_reference.uncertainty.is_exact()
        && input.drain_current_rms.uncertainty.is_exact()
        && input.r_theta_jc.uncertainty.is_exact()
        && r_cs_est.uncertainty.is_exact()
        && input.r_theta_sa.uncertainty.is_exact()
        && input.temp_coefficient.uncertainty.is_exact()
        && input.ambient_temperature.uncertainty.is_exact()
    {
        // Exact data is not a reason to invent spread.
    } else {
        warnings.push(warning(
            "uncertainty.envelope",
            "uncertainty propagation uses worst-case interval arithmetic; corner pairing is conservative and is not an independence proof",
            WarningSeverity::Info,
        ));
    }
    if !input.max_junction_temperature.is_known() {
        warnings.push(warning(
            "margin.unavailable",
            "maximum junction temperature is unavailable; thermal margin is not established",
            WarningSeverity::Warning,
        ));
    }

    let mut bcs = BTreeMap::new();
    bcs.insert(
        "drain_current_rms".to_string(),
        boundary_value(&input.drain_current_rms.nominal, context)?,
    );
    bcs.insert(
        "rds_on_reference".to_string(),
        boundary_value(&input.rds_on_reference.nominal, context)?,
    );
    bcs.insert(
        "reference_temperature".to_string(),
        boundary_value(&input.reference_temperature, context)?,
    );
    bcs.insert(
        "ambient_temperature".to_string(),
        boundary_value(&input.ambient_temperature.nominal, context)?,
    );
    bcs.insert(
        "r_theta_jc".to_string(),
        boundary_value(&input.r_theta_jc.nominal, context)?,
    );
    bcs.insert(
        "r_theta_cs".to_string(),
        boundary_value(&r_cs_est.nominal, context)?,
    );
    bcs.insert(
        "r_theta_sa".to_string(),
        boundary_value(&input.r_theta_sa.nominal, context)?,
    );

    let tj_uncertainty = if input.drain_current_rms.uncertainty.is_exact()
        && input.rds_on_reference.uncertainty.is_exact()
        && input.temp_coefficient.uncertainty.is_exact()
        && input.ambient_temperature.uncertainty.is_exact()
        && input.r_theta_jc.uncertainty.is_exact()
        && r_cs_est.uncertainty.is_exact()
        && input.r_theta_sa.uncertainty.is_exact()
    {
        Uncertainty::Exact
    } else {
        Uncertainty::Interval(tj_interval)
    };

    let mut results = vec![
        quantity_result("conduction_loss", watt(loss)?, Uncertainty::Exact),
        quantity_result("junction_temperature", celsius_from_k(tj)?, tj_uncertainty),
        quantity_result("rds_on_hot", ohm(rds_hot)?, Uncertainty::Exact),
        quantity_result(
            "thermal_resistance_total",
            Quantity::new(rth_nom, UnitRegistry::si().require("K/W")?)?,
            Uncertainty::Exact,
        ),
    ];
    match &thermal_margin {
        ValueState::Known(margin) => {
            results.push(quantity_result(
                "thermal_margin",
                margin.clone(),
                Uncertainty::Exact,
            ));
        }
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "thermal_margin",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }

    let receipt = build_receipt(
        MOSFET_THERMAL_MODEL,
        input,
        MODEL_EXPRESSION,
        &input,
        context,
        bcs,
        Convergence::iterative(1, residual, 1e-9, residual <= 1e-9),
        warnings.clone(),
        results,
        tj_uncertainty,
    )?;

    let cross_check = bisection(
        |temperature| {
            let rds_at = rds_nom * (1.0 + alpha_nom * (temperature - t_ref));
            let p = current_nom * current_nom * rds_at;
            temperature - (ta_nom + p * rth_nom)
        },
        ta_nom - 10.0,
        ta_nom + 2_000.0,
        1e-9,
        500,
    )
    .ok()
    .filter(|solution| solution.converged)
    .map(|solution| {
        CrossCheckEvidence::new(
            "independent bisection of the energy-balance residual",
            tj,
            solution.root,
            1e-7,
        )
    });

    Ok(ModelReport {
        status: ModelStatus::Solved,
        value: ValueState::known(MosfetThermalResult {
            conduction_loss: watt(loss)?,
            junction_temperature: celsius_from_k(tj)?,
            rds_on_hot: ohm(rds_hot)?,
            thermal_margin,
            loss_interval: Some(loss_interval),
            junction_interval_kelvin: Some(tj_interval),
            stability_denominator: denominator,
        }),
        refusal: None,
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: tj_uncertainty,
        cross_check,
    })
}

pub fn identity() -> engineering_core::AnalysisIdentity {
    model_identity(MOSFET_THERMAL_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;

#[cfg(test)]
mod tests {
    use engineering_core::{Provenance, ProvenanceKind, RevisionId, ValueState};
    use engineering_units::UnitRegistry;

    use super::*;

    fn context() -> ModelContext {
        ModelContext::new(
            RevisionId::new(1),
            Provenance::new(ProvenanceKind::Specification, "test").unwrap(),
        )
    }

    fn input() -> MosfetThermalInput {
        let units = UnitRegistry::si();
        MosfetThermalInput {
            drain_current_rms: Estimate::new(
                Quantity::new(20.0, units.require("A").unwrap()).unwrap(),
                Uncertainty::Exact,
            )
            .unwrap(),
            rds_on_reference: Estimate::new(
                Quantity::new(10.0, units.require("mohm").unwrap()).unwrap(),
                Uncertainty::Exact,
            )
            .unwrap(),
            temp_coefficient: Estimate::new(
                Quantity::new(0.004, units.require("1/K").unwrap()).unwrap(),
                Uncertainty::Exact,
            )
            .unwrap(),
            reference_temperature: Quantity::new(25.0, units.require("degC").unwrap()).unwrap(),
            ambient_temperature: Estimate::new(
                Quantity::new(25.0, units.require("degC").unwrap()).unwrap(),
                Uncertainty::Exact,
            )
            .unwrap(),
            r_theta_jc: Estimate::new(
                Quantity::new(1.0, units.require("K/W").unwrap()).unwrap(),
                Uncertainty::Exact,
            )
            .unwrap(),
            r_theta_cs: Some(
                Estimate::new(
                    Quantity::new(0.5, units.require("K/W").unwrap()).unwrap(),
                    Uncertainty::Exact,
                )
                .unwrap(),
            ),
            r_theta_sa: Estimate::new(
                Quantity::new(0.5, units.require("K/W").unwrap()).unwrap(),
                Uncertainty::Exact,
            )
            .unwrap(),
            max_junction_temperature: ValueState::known(
                Quantity::new(150.0, units.require("degC").unwrap()).unwrap(),
            ),
        }
    }

    #[test]
    fn coupled_fixed_point_matches_the_worked_case() {
        let report = mosfet_conduction_thermal(&input(), &context()).unwrap();
        let result = report.value.as_known().unwrap();
        assert!((result.conduction_loss.value() - 4.132_231_404_958_68).abs() < 1e-9);
        assert!((result.junction_temperature.value() - 33.264_462_809_917_4).abs() < 1e-8);
        assert!(result.thermal_margin.is_known());
        assert!(report.cross_check.as_ref().unwrap().passed);
    }

    #[test]
    fn missing_contact_resistance_is_refused() {
        let mut input = input();
        input.r_theta_cs = None;
        let report = mosfet_conduction_thermal(&input, &context()).unwrap();
        assert_eq!(report.status, ModelStatus::Refused);
        assert_eq!(
            report.refusal.as_ref().unwrap().code,
            RefusalCode::MissingInterfaceResistance
        );
    }

    #[test]
    fn thermal_runaway_is_refused_not_extrapolated() {
        let mut input = input();
        let units = UnitRegistry::si();
        input.rds_on_reference = Estimate::new(
            Quantity::new(0.5, units.require("ohm").unwrap()).unwrap(),
            Uncertainty::Exact,
        )
        .unwrap();
        let report = mosfet_conduction_thermal(&input, &context()).unwrap();
        assert_eq!(report.status, ModelStatus::Refused);
        assert_eq!(
            report.refusal.as_ref().unwrap().code,
            RefusalCode::ThermalRunaway
        );
    }
}
