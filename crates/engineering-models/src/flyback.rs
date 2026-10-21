use std::collections::BTreeMap;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ReceiptResult, ReceiptValue,
    ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_finite, require_positive, unavailable_result, value_in, warning, AssumptionRecord,
    CrossCheckEvidence, ModelContext, ModelError, ModelReport, ModelResult, ModelStatus, Refusal,
    RefusalCode, MODEL_VERSION,
};

pub const FLYBACK_MODEL: &str = "power-electronics/flyback-converter";
const MODEL_EXPRESSION: &str = "D_ccm = V_or/(Vin+V_or); D_dcm = sqrt(2 P L f)/Vin; E_lk = 0.5 L_lk I_pk^2; P_snub = E_lk f V_cl/(V_cl-V_or); V_ds,pk = Vin+V_cl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlybackMode {
    Ccm,
    Dcm,
    Boundary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlybackInput {
    pub input_voltage: Quantity,
    pub output_voltage: Quantity,
    pub output_current: Quantity,
    pub switching_frequency: Quantity,
    pub primary_inductance: Quantity,
    /// N_primary / N_secondary.
    pub turns_ratio: f64,
    pub leakage_inductance: Option<Quantity>,
    pub clamp_voltage: Option<Quantity>,
    pub clamp_ripple_fraction: f64,
    pub output_capacitance: Option<Quantity>,
    pub output_esr: Option<Quantity>,
    pub vds_rating: Option<Quantity>,
    pub voltage_derating: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlybackResult {
    pub mode: FlybackMode,
    pub duty: f64,
    pub reflected_output_voltage: Quantity,
    pub primary_peak_current: Quantity,
    pub primary_valley_current: Quantity,
    pub primary_rms_current: Quantity,
    pub secondary_peak_current: Quantity,
    pub secondary_rms_current: Quantity,
    pub leakage_energy_per_cycle: ValueState<Quantity>,
    pub snubber_loss: ValueState<Quantity>,
    pub clamp_resistance: ValueState<Quantity>,
    pub clamp_capacitance_min: ValueState<Quantity>,
    pub vds_peak: ValueState<Quantity>,
    pub voltage_margin: ValueState<Quantity>,
    pub overstress: Option<bool>,
    pub output_ripple: ValueState<Quantity>,
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
            "asm-lossless-mode-selection",
            "The CCM/DCM boundary and primary current are computed from ideal power transfer; conduction and switching losses are reported separately and do not move the mode boundary.",
            "Power-converters knowledge pack: ideal flyback mode selection.",
        ),
        assumption(
            "asm-constant-inductance",
            "Primary magnetizing inductance is constant over the switching cycle and the leakage inductance is a lumped turn-off element.",
            "Power-converters knowledge pack: lumped flyback model.",
        ),
        assumption(
            "asm-ideal-clamp",
            "The RCD clamp holds the primary winding at the declared clamp voltage during leakage-energy transfer; ringing and diode reverse recovery are outside the model.",
            "Power-converters knowledge pack: RCD clamp model.",
        ),
        assumption(
            "asm-ripple-superposition",
            "Output ripple is the sum of the capacitive charge ripple I_out*D/(C*f) and the ESR step ESR*I_sec,pk.",
            "Power-converters knowledge pack: output-ripple estimate.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "V_or = n V_out",
            Dimension::VOLTAGE,
            Dimension::SCALAR.checked_mul(Dimension::VOLTAGE)?,
        )?,
        DimensionCheck::new(
            "D = V_or/(Vin+V_or)",
            Dimension::SCALAR,
            Dimension::VOLTAGE.checked_div(Dimension::VOLTAGE)?,
        )?,
        DimensionCheck::new(
            "E_lk = 0.5 L I^2",
            Dimension::ENERGY,
            Dimension::INDUCTANCE.checked_mul(Dimension::CURRENT.checked_powi(2)?)?,
        )?,
        DimensionCheck::new(
            "P_snub = E_lk f",
            Dimension::POWER,
            Dimension::ENERGY.checked_mul(Dimension::FREQUENCY)?,
        )?,
        DimensionCheck::new(
            "R_clamp = V_cl^2/P_snub",
            Dimension::RESISTANCE,
            Dimension::VOLTAGE
                .checked_powi(2)?
                .checked_div(Dimension::POWER)?,
        )?,
        DimensionCheck::new(
            "C_clamp = I/(f dV)",
            Dimension::CAPACITANCE,
            Dimension::CURRENT
                .checked_div(Dimension::FREQUENCY.checked_mul(Dimension::VOLTAGE)?)?,
        )?,
        DimensionCheck::new(
            "V_ds = Vin + V_cl",
            Dimension::VOLTAGE,
            Dimension::VOLTAGE.checked_add(Dimension::VOLTAGE)?,
        )?,
        DimensionCheck::new(
            "dV_out = I D/(C f)",
            Dimension::VOLTAGE,
            Dimension::CURRENT
                .checked_div(Dimension::CAPACITANCE.checked_mul(Dimension::FREQUENCY)?)?,
        )?,
        DimensionCheck::new(
            "dV_esr = ESR I",
            Dimension::VOLTAGE,
            Dimension::RESISTANCE.checked_mul(Dimension::CURRENT)?,
        )?,
    ])
}

fn volt(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("V")?)?)
}

fn ampere(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("A")?)?)
}

fn joule(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("J")?)?)
}

fn watt(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("W")?)?)
}

fn ohm(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("ohm")?)?)
}

fn farad(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("F")?)?)
}

fn unavailable<T>(reason: impl Into<String>, required: Vec<String>) -> ModelResult<ValueState<T>> {
    Ok(ValueState::unavailable(reason, required)?)
}

fn rms_triangle(peak: f64, valley: f64, duty: f64) -> f64 {
    (duty / 3.0 * (peak * peak + peak * valley + valley * valley))
        .max(0.0)
        .sqrt()
}

fn refusal_report(
    input: &FlybackInput,
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<FlybackResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    bcs.insert(
        "input_voltage".to_string(),
        boundary_value(&input.input_voltage, context)?,
    );
    bcs.insert(
        "output_voltage".to_string(),
        boundary_value(&input.output_voltage, context)?,
    );
    bcs.insert(
        "switching_frequency".to_string(),
        boundary_value(&input.switching_frequency, context)?,
    );
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let results = vec![unavailable_result(
        "operating_point",
        reason.clone(),
        vec!["a flyback operating point inside the declared validity envelope".to_string()],
    )?];
    let receipt = build_receipt(
        FLYBACK_MODEL,
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
        value: ValueState::unavailable(reason, vec!["valid flyback operating point".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

pub fn flyback_converter(
    input: &FlybackInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<FlybackResult>> {
    let checks = checks()?;
    for (name, quantity, dimension) in [
        ("input_voltage", &input.input_voltage, Dimension::VOLTAGE),
        ("output_voltage", &input.output_voltage, Dimension::VOLTAGE),
        ("output_current", &input.output_current, Dimension::CURRENT),
        (
            "switching_frequency",
            &input.switching_frequency,
            Dimension::FREQUENCY,
        ),
        (
            "primary_inductance",
            &input.primary_inductance,
            Dimension::INDUCTANCE,
        ),
    ] {
        require_dimension(name, quantity, dimension)?;
    }
    let vin = value_in(&input.input_voltage, "V")?;
    let vout = value_in(&input.output_voltage, "V")?;
    let iout = value_in(&input.output_current, "A")?;
    let fsw = value_in(&input.switching_frequency, "Hz")?;
    let lp = value_in(&input.primary_inductance, "H")?;
    require_positive("input_voltage", vin)?;
    require_positive("output_voltage", vout)?;
    require_positive("output_current", iout)?;
    require_positive("switching_frequency", fsw)?;
    require_positive("primary_inductance", lp)?;
    require_positive("turns_ratio", input.turns_ratio)?;
    require_finite("clamp_ripple_fraction", input.clamp_ripple_fraction)?;
    require_finite("voltage_derating", input.voltage_derating)?;
    if input.clamp_ripple_fraction <= 0.0 {
        return Err(ModelError::InvalidInput {
            field: "clamp_ripple_fraction",
            reason: "must be > 0".to_string(),
        });
    }
    if !(0.0..=1.0).contains(&input.voltage_derating) || input.voltage_derating == 0.0 {
        return Err(ModelError::InvalidInput {
            field: "voltage_derating",
            reason: "must be in (0, 1]".to_string(),
        });
    }
    let vor = input.turns_ratio * vout;
    let pout = vout * iout;
    let d_ccm = vor / (vin + vor);
    let d_dcm = (2.0 * pout * lp * fsw).sqrt() / vin;
    let ipk_dcm = vin * d_dcm / (lp * fsw);
    let d_reset_dcm = ipk_dcm * lp * fsw / vor;
    let boundary_slack = d_dcm + d_reset_dcm - 1.0;
    let mode = if boundary_slack.abs() <= 1e-9 {
        FlybackMode::Boundary
    } else if boundary_slack < 0.0 {
        FlybackMode::Dcm
    } else {
        FlybackMode::Ccm
    };
    let (duty, ipk, ivalley, doff) = match mode {
        FlybackMode::Ccm => {
            let iin_avg = pout / vin;
            let i_on_avg = iin_avg / d_ccm;
            let ripple = vin * d_ccm / (lp * fsw);
            let peak = i_on_avg + ripple / 2.0;
            let valley = i_on_avg - ripple / 2.0;
            (d_ccm, peak, valley.max(0.0), 1.0 - d_ccm)
        }
        FlybackMode::Dcm | FlybackMode::Boundary => (d_dcm, ipk_dcm, 0.0, d_reset_dcm),
    };
    if duty >= 0.9 {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                format!(
                    "required duty {duty:.6} is at or above the 0.90 validity boundary; the ideal flyback model cannot establish adequate reset at this operating point"
                ),
                vec!["a lower turns ratio, higher input voltage or a different conversion topology".to_string()],
            ),
            checks,
        );
    }
    let ipk_secondary = input.turns_ratio * ipk;
    let ivalley_secondary = input.turns_ratio * ivalley;
    let primary_rms = rms_triangle(ipk, ivalley, duty);
    let secondary_rms = rms_triangle(ipk_secondary, ivalley_secondary, doff);
    let average_secondary = 0.5 * (ipk_secondary + ivalley_secondary) * doff;
    let power_balance = vin * (0.5 * (ipk + ivalley) * duty) - pout;

    let leakage = input
        .leakage_inductance
        .as_ref()
        .map(|quantity| {
            require_dimension("leakage_inductance", quantity, Dimension::INDUCTANCE)?;
            value_in(quantity, "H")
        })
        .transpose()?;
    if leakage.is_some_and(|value| value < 0.0) {
        return Err(ModelError::InvalidInput {
            field: "leakage_inductance",
            reason: "must be non-negative".to_string(),
        });
    }
    let clamp = input
        .clamp_voltage
        .as_ref()
        .map(|quantity| {
            require_dimension("clamp_voltage", quantity, Dimension::VOLTAGE)?;
            value_in(quantity, "V")
        })
        .transpose()?;
    if let Some(vclamp) = clamp {
        require_positive("clamp_voltage", vclamp)?;
        if vclamp <= vor {
            return refusal_report(
                input,
                context,
                Refusal::new(
                    RefusalCode::OutsideValidity,
                    format!(
                        "clamp voltage {vclamp} V does not exceed the reflected output voltage {vor} V; the RCD clamp cannot reset the transformer"
                    ),
                    vec!["a clamp voltage above the reflected output voltage".to_string()],
                ),
                checks,
            );
        }
    }
    let leakage_energy = match leakage {
        Some(llk) => ValueState::Known(joule(0.5 * llk * ipk * ipk)?),
        None => unavailable(
            "leakage inductance is not established; leakage energy, clamp sizing and the leakage-driven Vds stress are unavailable",
            vec!["revision-bound transformer leakage inductance".to_string()],
        )?,
    };
    let (snubber_loss, clamp_resistance, clamp_capacitance, vds_peak) = match (leakage, clamp) {
        (Some(llk), Some(vclamp)) => {
            let energy = 0.5 * llk * ipk * ipk;
            let loss = energy * fsw * vclamp / (vclamp - vor);
            let resistance = vclamp * vclamp / loss;
            let capacitance = ipk / (fsw * input.clamp_ripple_fraction * vclamp);
            (
                ValueState::Known(watt(loss)?),
                ValueState::Known(ohm(resistance)?),
                ValueState::Known(farad(capacitance)?),
                ValueState::Known(volt(vin + vclamp)?),
            )
        }
        (Some(_), None) => (
            unavailable(
                "clamp voltage is not established; snubber loss and RCD sizing are unavailable",
                vec!["RCD clamp voltage or an equivalent measured clamp".to_string()],
            )?,
            unavailable(
                "clamp voltage is not established; clamp resistance is unavailable",
                vec!["RCD clamp voltage".to_string()],
            )?,
            unavailable(
                "clamp voltage is not established; clamp capacitance is unavailable",
                vec!["RCD clamp voltage".to_string()],
            )?,
            unavailable(
                "clamp voltage is not established; the clamped Vds peak is unavailable",
                vec!["RCD clamp voltage".to_string()],
            )?,
        ),
        (None, _) => (
            unavailable(
                "leakage inductance is not established; snubber loss is unavailable",
                vec!["transformer leakage inductance".to_string()],
            )?,
            unavailable(
                "leakage inductance is not established; clamp resistance is unavailable",
                vec!["transformer leakage inductance".to_string()],
            )?,
            unavailable(
                "leakage inductance is not established; clamp capacitance is unavailable",
                vec!["transformer leakage inductance".to_string()],
            )?,
            unavailable(
                "leakage inductance is not established; leakage-driven Vds stress is unavailable",
                vec!["transformer leakage inductance".to_string()],
            )?,
        ),
    };
    let (voltage_margin, overstress) = match (&vds_peak, &input.vds_rating) {
        (ValueState::Known(peak), Some(rating)) => {
            require_dimension("vds_rating", rating, Dimension::VOLTAGE)?;
            let rating_v = value_in(rating, "V")?;
            require_positive("vds_rating", rating_v)?;
            let peak_v = value_in(peak, "V")?;
            let margin = rating_v * input.voltage_derating - peak_v;
            (ValueState::Known(volt(margin)?), Some(margin < 0.0))
        }
        (ValueState::Known(peak), None) => {
            let peak_v = value_in(peak, "V")?;
            let _ = peak_v;
            (
                unavailable(
                    "MOSFET Vds rating is not established; the voltage margin is unavailable",
                    vec!["revision-bound MOSFET Vds rating".to_string()],
                )?,
                None,
            )
        }
        (ValueState::Unavailable(reason), _) => (ValueState::Unavailable(reason.clone()), None),
    };
    let output_ripple = match (&input.output_capacitance, &input.output_esr) {
        (Some(cout), Some(esr)) => {
            require_dimension("output_capacitance", cout, Dimension::CAPACITANCE)?;
            require_dimension("output_esr", esr, Dimension::RESISTANCE)?;
            let cout_f = value_in(cout, "F")?;
            let esr_ohm = value_in(esr, "ohm")?;
            require_positive("output_capacitance", cout_f)?;
            if esr_ohm < 0.0 {
                return Err(ModelError::InvalidInput {
                    field: "output_esr",
                    reason: "must be non-negative".to_string(),
                });
            }
            let capacitive = iout * duty / (cout_f * fsw);
            let resistive = esr_ohm * ipk_secondary;
            ValueState::Known(volt(capacitive + resistive)?)
        }
        (None, _) => unavailable(
            "output capacitance is not established; output ripple is unavailable",
            vec!["output capacitance including derating".to_string()],
        )?,
        (_, None) => unavailable(
            "output capacitor ESR is not established; output ripple is unavailable",
            vec!["output capacitor ESR at the switching frequency".to_string()],
        )?,
    };

    let mut warnings = Vec::new();
    if overstress == Some(true) {
        warnings.push(warning(
            "vds.overstress",
            "the clamped Vds peak exceeds the derated MOSFET rating; the design is rejected",
            WarningSeverity::Critical,
        ));
    }
    if matches!(mode, FlybackMode::Boundary) {
        warnings.push(warning(
            "mode.boundary",
            "the operating point lies on the CCM/DCM boundary; small changes in load or inductance change the mode",
            WarningSeverity::Info,
        ));
    }
    if !leakage_energy.is_known() {
        warnings.push(warning(
            "leakage.unavailable",
            "leakage inductance is unavailable; leakage-driven stress is not bounded by this run",
            WarningSeverity::Warning,
        ));
    }
    if !output_ripple.is_known() {
        warnings.push(warning(
            "ripple.unavailable",
            "output ripple is unavailable because the capacitor data is incomplete",
            WarningSeverity::Warning,
        ));
    }
    let mut bcs = BTreeMap::new();
    bcs.insert(
        "input_voltage".to_string(),
        boundary_value(&input.input_voltage, context)?,
    );
    bcs.insert(
        "output_voltage".to_string(),
        boundary_value(&input.output_voltage, context)?,
    );
    bcs.insert(
        "switching_frequency".to_string(),
        boundary_value(&input.switching_frequency, context)?,
    );
    bcs.insert(
        "primary_inductance".to_string(),
        boundary_value(&input.primary_inductance, context)?,
    );
    if let Some(llk) = &input.leakage_inductance {
        bcs.insert(
            "leakage_inductance".to_string(),
            boundary_value(llk, context)?,
        );
    }
    if let Some(vclamp) = &input.clamp_voltage {
        bcs.insert(
            "clamp_voltage".to_string(),
            boundary_value(vclamp, context)?,
        );
    }
    let mut results: Vec<ReceiptResult> = vec![
        ReceiptResult {
            name: "mode".to_string(),
            value: ReceiptValue::Text(format!("{mode:?}")),
            uncertainty: Uncertainty::Exact,
        },
        quantity_result("duty", Quantity::dimensionless(duty)?, Uncertainty::Exact),
        quantity_result("reflected_output_voltage", volt(vor)?, Uncertainty::Exact),
        quantity_result("primary_peak_current", ampere(ipk)?, Uncertainty::Exact),
        quantity_result(
            "primary_valley_current",
            ampere(ivalley)?,
            Uncertainty::Exact,
        ),
        quantity_result(
            "primary_rms_current",
            ampere(primary_rms)?,
            Uncertainty::Exact,
        ),
        quantity_result(
            "secondary_peak_current",
            ampere(ipk_secondary)?,
            Uncertainty::Exact,
        ),
        quantity_result(
            "secondary_rms_current",
            ampere(secondary_rms)?,
            Uncertainty::Exact,
        ),
    ];
    for (name, value) in [
        ("leakage_energy_per_cycle", &leakage_energy),
        ("snubber_loss", &snubber_loss),
        ("clamp_resistance", &clamp_resistance),
        ("clamp_capacitance_min", &clamp_capacitance),
        ("vds_peak", &vds_peak),
        ("voltage_margin", &voltage_margin),
        ("output_ripple", &output_ripple),
    ] {
        match value {
            ValueState::Known(quantity) => {
                results.push(quantity_result(name, quantity.clone(), Uncertainty::Exact))
            }
            ValueState::Unavailable(reason) => results.push(unavailable_result(
                name,
                reason.reason.clone(),
                reason.required.clone(),
            )?),
        }
    }
    let cross_check = if (average_secondary - iout).abs() <= 1e-6 * iout.max(1.0)
        && power_balance.abs() <= 1e-6 * pout.max(1.0)
    {
        Some(CrossCheckEvidence::new(
            "average secondary current equals the load current",
            average_secondary,
            iout,
            1e-6 * iout.max(1.0),
        ))
    } else {
        Some(CrossCheckEvidence::new(
            "average secondary current disagrees with the load current",
            average_secondary,
            iout,
            1e-6 * iout.max(1.0),
        ))
    };
    let receipt = build_receipt(
        FLYBACK_MODEL,
        input,
        MODEL_EXPRESSION,
        &input,
        context,
        bcs,
        Convergence::exact(),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Solved,
        value: ValueState::known(FlybackResult {
            mode,
            duty,
            reflected_output_voltage: volt(vor)?,
            primary_peak_current: ampere(ipk)?,
            primary_valley_current: ampere(ivalley)?,
            primary_rms_current: ampere(primary_rms)?,
            secondary_peak_current: ampere(ipk_secondary)?,
            secondary_rms_current: ampere(secondary_rms)?,
            leakage_energy_per_cycle: leakage_energy,
            snubber_loss,
            clamp_resistance,
            clamp_capacitance_min: clamp_capacitance,
            vds_peak,
            voltage_margin,
            overstress,
            output_ripple,
        }),
        refusal: None,
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check,
    })
}

pub fn identity() -> engineering_core::AnalysisIdentity {
    model_identity(FLYBACK_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;
