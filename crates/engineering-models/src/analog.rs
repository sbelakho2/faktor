use std::collections::BTreeMap;
use std::f64::consts::PI;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_finite, require_positive, unavailable_result, value_in, warning, AssumptionRecord,
    CrossCheckEvidence, ModelContext, ModelError, ModelReport, ModelResult, ModelStatus, Refusal,
    RefusalCode, MODEL_VERSION,
};

pub const OPAMP_STABILITY_MODEL: &str = "electrical/opamp-loop-stability";
const MODEL_EXPRESSION: &str = "T(s) = (A0/((1+s/w_p1)(1+s/w_p2)(1+s/w_out))) * (beta0(1+s/w_z)/(1+s/w_in)); PM = 180 + arg T(j w_c)";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpAmpStabilityInput {
    pub open_loop_gain: f64,
    pub first_pole: Quantity,
    pub second_pole: Quantity,
    pub output_resistance: Option<Quantity>,
    pub load_capacitance: Option<Quantity>,
    pub opamp_output_capacitance: Option<Quantity>,
    pub input_capacitance: Option<Quantity>,
    pub feedback_resistance: Option<Quantity>,
    pub feedback_capacitor: Option<Quantity>,
    pub noise_gain: f64,
    pub required_phase_margin_deg: f64,
    pub required_gain_margin_db: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpAmpStabilityResult {
    pub dc_loop_gain: f64,
    pub crossover_frequency: ValueState<Quantity>,
    pub phase_margin: ValueState<Quantity>,
    pub gain_margin_db: ValueState<Quantity>,
    pub output_pole_hz: f64,
    pub input_pole_hz: Option<f64>,
    pub zero_hz: Option<f64>,
    pub stable: bool,
    pub meets_margin: bool,
    pub compensation_required: bool,
    pub required_noise_gain: Option<f64>,
    pub max_stable_load_capacitance: Option<Quantity>,
    pub compensation_note: String,
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
            "asm-linear-loop",
            "The op-amp loop is linear and time-invariant with two device poles plus one output pole and one feedback-network pole.",
            "Analog-stability knowledge pack: linear loop-gain model.",
        ),
        assumption(
            "asm-dominant-poles",
            "Higher-order device poles and slew-rate nonlinearity are outside the model; the declared second pole is the dominant non-ideal pole.",
            "Analog-stability knowledge pack: validity envelope.",
        ),
        assumption(
            "asm-capacitive-load",
            "The load is a capacitive load in parallel with the feedback network; the output pole is 1/(2*pi*R_out*(C_load+C_out)).",
            "Analog-stability knowledge pack: output-pole model.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "w_p = 1/(R C)",
            Dimension::FREQUENCY,
            Dimension::SCALAR
                .checked_div(Dimension::RESISTANCE.checked_mul(Dimension::CAPACITANCE)?)?,
        )?,
        DimensionCheck::new(
            "T(j w) is dimensionless",
            Dimension::SCALAR,
            Dimension::SCALAR.checked_mul(Dimension::SCALAR)?,
        )?,
        DimensionCheck::new("PM is an angle", Dimension::SCALAR, Dimension::SCALAR)?,
        DimensionCheck::new(
            "C_f = 1/(2 pi R_f f_z)",
            Dimension::CAPACITANCE,
            Dimension::SCALAR
                .checked_div(Dimension::RESISTANCE.checked_mul(Dimension::FREQUENCY)?)?,
        )?,
    ])
}

fn hz(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("Hz")?)?)
}

fn deg(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("deg")?)?)
}

fn db(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("dB")?)?)
}

fn farad(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("F")?)?)
}

#[derive(Debug, Clone, Copy)]
struct LoopShape {
    a0: f64,
    beta0: f64,
    fp1: f64,
    fp2: f64,
    f_out: f64,
    f_in: f64,
    f_zero: f64,
    f_fb: f64,
}

impl LoopShape {
    fn response(&self, frequency: f64) -> (f64, f64) {
        let x = frequency;
        let magnitude = self.a0 * self.beta0 * (1.0 + (x / self.f_zero).powi(2)).sqrt()
            / ((1.0 + (x / self.fp1).powi(2)).sqrt()
                * (1.0 + (x / self.fp2).powi(2)).sqrt()
                * (1.0 + (x / self.f_out).powi(2)).sqrt()
                * (1.0 + (x / self.f_in).powi(2)).sqrt()
                * (1.0 + (x / self.f_fb).powi(2)).sqrt());
        let phase = -(x / self.fp1).atan() - (x / self.fp2).atan() - (x / self.f_out).atan()
            + (x / self.f_zero).atan()
            - (x / self.f_in).atan()
            - (x / self.f_fb).atan();
        (magnitude, phase)
    }
}

fn phase_of_complex(re: f64, im: f64) -> f64 {
    im.atan2(re)
}

fn complex_response(shape: &LoopShape, frequency: f64) -> (f64, f64) {
    let w1 = 2.0 * PI * shape.fp1;
    let w2 = 2.0 * PI * shape.fp2;
    let wo = 2.0 * PI * shape.f_out;
    let wi = 2.0 * PI * shape.f_in;
    let wz = 2.0 * PI * shape.f_zero;
    let wfb = 2.0 * PI * shape.f_fb;
    let w = 2.0 * PI * frequency;
    let pole = |wp: f64| (1.0, w / wp);
    let (a_re, a_im) = pole(w1);
    let (b_re, b_im) = pole(w2);
    let (c_re, c_im) = pole(wo);
    let (d_re, d_im) = pole(wi);
    let (e_re, e_im) = pole(wfb);
    let (z_re, z_im) = pole(wz);
    let den_re = a_re * b_re - a_im * b_im;
    let den_im = a_re * b_im + a_im * b_re;
    let den2_re = den_re * c_re - den_im * c_im;
    let den2_im = den_re * c_im + den_im * c_re;
    let den3_re = den2_re * d_re - den2_im * d_im;
    let den3_im = den2_re * d_im + den2_im * d_re;
    let den4_re = den3_re * e_re - den3_im * e_im;
    let den4_im = den3_re * e_im + den3_im * e_re;
    let num_re = z_re;
    let num_im = z_im;
    let scale = shape.a0 * shape.beta0;
    let num_s_re = scale * num_re;
    let num_s_im = scale * num_im;
    let den_abs2 = den4_re * den4_re + den4_im * den4_im;
    let re = (num_s_re * den4_re + num_s_im * den4_im) / den_abs2;
    let im = (num_s_im * den4_re - num_s_re * den4_im) / den_abs2;
    (re, im)
}

fn find_crossover(shape: &LoopShape, f_min: f64, f_max: f64) -> Option<f64> {
    const SAMPLES: usize = 4096;
    let log_min = f_min.ln();
    let log_max = f_max.ln();
    let step = (log_max - log_min) / SAMPLES as f64;
    let mut previous = shape.response(f_min).0;
    if previous <= 1.0 {
        return Some(f_min);
    }
    for index in 1..=SAMPLES {
        let frequency = (log_min + step * index as f64).exp();
        let magnitude = shape.response(frequency).0;
        if magnitude <= 1.0 {
            let mut low = (log_min + step * (index - 1) as f64).exp();
            let mut high = frequency;
            for _ in 0..200 {
                let middle = (low * high).sqrt();
                if shape.response(middle).0 > 1.0 {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            return Some((low * high).sqrt());
        }
        previous = magnitude;
    }
    let _ = previous;
    None
}

fn find_phase_crossover(shape: &LoopShape, f_min: f64, f_max: f64) -> Option<f64> {
    const SAMPLES: usize = 4096;
    let log_min = f_min.ln();
    let log_max = f_max.ln();
    let step = (log_max - log_min) / SAMPLES as f64;
    let mut previous_phase = shape.response(f_min).1;
    for index in 1..=SAMPLES {
        let frequency = (log_min + step * index as f64).exp();
        let phase = shape.response(frequency).1;
        if previous_phase > -PI && phase <= -PI {
            let mut low = (log_min + step * (index - 1) as f64).exp();
            let mut high = frequency;
            for _ in 0..200 {
                let middle = (low * high).sqrt();
                if shape.response(middle).1 > -PI {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            return Some((low * high).sqrt());
        }
        previous_phase = phase;
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn phase_margin_for_shape(
    input: &OpAmpStabilityInput,
    fp1: f64,
    fp2: f64,
    f_out: f64,
    f_in: f64,
    f_zero: f64,
    f_fb: f64,
    noise_gain: f64,
) -> Option<f64> {
    let shape = LoopShape {
        a0: input.open_loop_gain,
        beta0: 1.0 / noise_gain,
        fp1,
        fp2,
        f_out,
        f_in,
        f_zero,
        f_fb,
    };
    let f_min = (fp1 * 1e-6).max(1e-9);
    let f_max = (fp1
        .max(fp2)
        .max(f_out)
        .max(if f_in.is_finite() { f_in } else { 0.0 })
        .max(if f_zero.is_finite() { f_zero } else { 0.0 })
        .max(if f_fb.is_finite() { f_fb } else { 0.0 })
        .max(1.0))
        * 1e6;
    let crossover = find_crossover(&shape, f_min, f_max.min(1e18))?;
    let (_, phase) = shape.response(crossover);
    Some(180.0 + phase.to_degrees())
}

fn refusal_report(
    input: &OpAmpStabilityInput,
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<OpAmpStabilityResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    bcs.insert(
        "first_pole".to_string(),
        boundary_value(&input.first_pole, context)?,
    );
    bcs.insert(
        "second_pole".to_string(),
        boundary_value(&input.second_pole, context)?,
    );
    let results = vec![unavailable_result(
        "phase_margin",
        reason.clone(),
        vec!["a loop model inside its declared validity envelope".to_string()],
    )?];
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let receipt = build_receipt(
        OPAMP_STABILITY_MODEL,
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
        value: ValueState::unavailable(reason, vec!["valid loop-stability model".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

fn unavailable_report(
    input: &OpAmpStabilityInput,
    context: &ModelContext,
    reason: String,
    required: Vec<String>,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<OpAmpStabilityResult>> {
    let mut bcs = BTreeMap::new();
    bcs.insert(
        "first_pole".to_string(),
        boundary_value(&input.first_pole, context)?,
    );
    bcs.insert(
        "second_pole".to_string(),
        boundary_value(&input.second_pole, context)?,
    );
    let warnings = vec![warning(
        "model.unavailable",
        reason.clone(),
        WarningSeverity::Warning,
    )];
    let results = vec![unavailable_result(
        "phase_margin",
        reason.clone(),
        required.clone(),
    )?];
    let receipt = build_receipt(
        OPAMP_STABILITY_MODEL,
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
        status: ModelStatus::Unavailable,
        value: ValueState::unavailable(reason, required)?,
        refusal: None,
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

enum OpampValidation {
    Valid {
        fp1: f64,
        fp2: f64,
        checks: Vec<DimensionCheck>,
    },
    Report(Box<ModelReport<OpAmpStabilityResult>>),
}

fn validate_opamp_input(
    input: &OpAmpStabilityInput,
    context: &ModelContext,
    checks: Vec<DimensionCheck>,
) -> ModelResult<OpampValidation> {
    require_dimension("first_pole", &input.first_pole, Dimension::FREQUENCY)?;
    require_dimension("second_pole", &input.second_pole, Dimension::FREQUENCY)?;
    let fp1 = value_in(&input.first_pole, "Hz")?;
    let fp2 = value_in(&input.second_pole, "Hz")?;
    require_positive("first_pole", fp1)?;
    require_positive("second_pole", fp2)?;
    require_positive("open_loop_gain", input.open_loop_gain)?;
    require_positive("noise_gain", input.noise_gain)?;
    require_finite("required_phase_margin_deg", input.required_phase_margin_deg)?;
    require_finite("required_gain_margin_db", input.required_gain_margin_db)?;
    if input.open_loop_gain <= 1.0 {
        return Ok(OpampValidation::Report(Box::new(refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                "the linear loop model requires open-loop gain greater than one",
                vec!["device open-loop gain above unity".to_string()],
            ),
            checks,
        )?)));
    }
    if input.noise_gain < 1.0 {
        return Ok(OpampValidation::Report(Box::new(refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                format!(
                    "noise gain {} is below unity; the feedback factor would exceed one",
                    input.noise_gain
                ),
                vec!["a passive feedback network with noise gain >= 1".to_string()],
            ),
            checks,
        )?)));
    }
    if fp2 <= fp1 {
        return Ok(OpampValidation::Report(Box::new(refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                format!(
                    "second pole {fp2} Hz is not above the first pole {fp1} Hz; the two-pole ordering is outside the model"
                ),
                vec!["ordered device pole frequencies from the datasheet/loop measurement".to_string()],
            ),
            checks,
        )?)));
    }
    if !(0.0..90.0).contains(&input.required_phase_margin_deg)
        || input.required_gain_margin_db < 0.0
    {
        return Ok(OpampValidation::Report(Box::new(refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                "the required phase/gain margin acceptance boundary is outside the linear-stability model",
                vec!["a required phase margin in [0, 90) deg and gain margin >= 0 dB".to_string()],
            ),
            checks,
        )?)));
    }
    Ok(OpampValidation::Valid { fp1, fp2, checks })
}

pub fn opamp_loop_stability(
    input: &OpAmpStabilityInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<OpAmpStabilityResult>> {
    let (fp1, fp2, checks) = match validate_opamp_input(input, context, checks()?)? {
        OpampValidation::Valid { fp1, fp2, checks } => (fp1, fp2, checks),
        OpampValidation::Report(report) => return Ok(*report),
    };

    let Some(r_out_q) = &input.output_resistance else {
        return unavailable_report(
            input,
            context,
            "output resistance is not established; the output pole and phase margin are unavailable".to_string(),
            vec!["revision-bound op-amp output resistance".to_string()],
            checks,
        );
    };
    let Some(c_load_q) = &input.load_capacitance else {
        return unavailable_report(
            input,
            context,
            "load capacitance is not established; the output pole and phase margin are unavailable"
                .to_string(),
            vec!["load capacitance including PCB and cable parasitics".to_string()],
            checks,
        );
    };
    let Some(c_out_q) = &input.opamp_output_capacitance else {
        return unavailable_report(
            input,
            context,
            "op-amp output capacitance is not established; the output pole and phase margin are unavailable".to_string(),
            vec!["device output capacitance".to_string()],
            checks,
        );
    };
    let Some(c_in_q) = &input.input_capacitance else {
        return unavailable_report(
            input,
            context,
            "input capacitance at the inverting node is not established; the feedback pole is unavailable".to_string(),
            vec!["device input/differential capacitance".to_string()],
            checks,
        );
    };
    let Some(rf_q) = &input.feedback_resistance else {
        return unavailable_report(
            input,
            context,
            "feedback resistance is not established; the input pole and compensation sizing are unavailable".to_string(),
            vec!["feedback network resistance values".to_string()],
            checks,
        );
    };
    for (name, quantity, dimension) in [
        ("output_resistance", r_out_q, Dimension::RESISTANCE),
        ("load_capacitance", c_load_q, Dimension::CAPACITANCE),
        ("opamp_output_capacitance", c_out_q, Dimension::CAPACITANCE),
        ("input_capacitance", c_in_q, Dimension::CAPACITANCE),
        ("feedback_resistance", rf_q, Dimension::RESISTANCE),
    ] {
        require_dimension(name, quantity, dimension)?;
    }
    let r_out = value_in(r_out_q, "ohm")?;
    let c_load = value_in(c_load_q, "F")?;
    let c_out = value_in(c_out_q, "F")?;
    let c_in = value_in(c_in_q, "F")?;
    let rf = value_in(rf_q, "ohm")?;
    require_positive("output_resistance", r_out)?;
    require_positive("feedback_resistance", rf)?;
    if c_load < 0.0 || c_out < 0.0 || c_in < 0.0 {
        return Err(ModelError::InvalidInput {
            field: "capacitance",
            reason: "capacitances must be non-negative".to_string(),
        });
    }
    let c_total = c_load + c_out;
    if c_total <= 0.0 {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                "the output pole model requires a positive total output capacitance",
                vec!["load plus device output capacitance greater than zero".to_string()],
            ),
            checks,
        );
    }
    let f_out = 1.0 / (2.0 * PI * r_out * c_total);
    let beta0 = 1.0 / input.noise_gain;
    let r_eq = rf * (input.noise_gain - 1.0) / input.noise_gain;
    let f_in = if c_in > 0.0 {
        if r_eq <= 0.0 {
            f64::INFINITY
        } else {
            1.0 / (2.0 * PI * r_eq * c_in)
        }
    } else {
        f64::INFINITY
    };
    let feedback_c = input
        .feedback_capacitor
        .as_ref()
        .map(|quantity| {
            require_dimension("feedback_capacitor", quantity, Dimension::CAPACITANCE)?;
            value_in(quantity, "F")
        })
        .transpose()?;
    if feedback_c.is_some_and(|value| value < 0.0) {
        return Err(ModelError::InvalidInput {
            field: "feedback_capacitor",
            reason: "feedback capacitance must be non-negative".to_string(),
        });
    }
    let f_zero = match feedback_c {
        Some(value) if value > 0.0 => 1.0 / (2.0 * PI * rf * value),
        _ => f64::INFINITY,
    };
    let f_fb = match feedback_c {
        Some(value) if value > 0.0 => {
            if r_eq > 0.0 {
                1.0 / (2.0 * PI * r_eq * value)
            } else {
                f_zero
            }
        }
        _ => f64::INFINITY,
    };
    let shape = LoopShape {
        a0: input.open_loop_gain,
        beta0,
        fp1,
        fp2,
        f_out,
        f_in,
        f_zero,
        f_fb,
    };
    let f_min = (fp1 * 1e-6).max(1e-9);
    let f_max = (fp1
        .max(fp2)
        .max(f_out)
        .max(if f_in.is_finite() { f_in } else { 0.0 })
        .max(if f_zero.is_finite() { f_zero } else { 0.0 })
        .max(if f_fb.is_finite() { f_fb } else { 0.0 })
        .max(1.0))
        * 1e6;
    let crossover = find_crossover(&shape, f_min, f_max.min(1e18));
    let phase_crossover = find_phase_crossover(&shape, f_min, f_max.min(1e18));
    let (phase_margin, gain_margin, stable, crossover_frequency) = match crossover {
        Some(frequency) => {
            let (_, phase) = shape.response(frequency);
            let pm = 180.0 + phase.to_degrees();
            let gm = phase_crossover
                .map(|phase_frequency| {
                    let magnitude = shape.response(phase_frequency).0;
                    -20.0 * magnitude.log10()
                })
                .unwrap_or(f64::INFINITY);
            let stable = pm > 0.0;
            (
                ValueState::Known(deg(pm)?),
                if gm.is_finite() {
                    ValueState::Known(db(gm)?)
                } else {
                    ValueState::unavailable(
                        "the loop phase never reaches -180 deg before the bounded frequency sweep; gain margin is unbounded in this model",
                        vec!["a measured high-frequency loop response".to_string()],
                    )?
                },
                stable,
                ValueState::Known(hz(frequency)?),
            )
        }
        None => (
            ValueState::unavailable(
                "loop gain does not cross unity inside the bounded frequency sweep; phase margin is undefined and the loop is treated as unstable",
                vec!["a loop-gain measurement extending beyond the modeled poles".to_string()],
            )?,
            ValueState::unavailable(
                "no unity-gain crossover exists, so gain margin cannot be evaluated",
                vec!["measured loop gain/phase".to_string()],
            )?,
            false,
            ValueState::unavailable(
                "no unity-gain crossover exists inside the bounded sweep",
                vec!["measured loop gain/phase".to_string()],
            )?,
        ),
    };
    let meets_margin = match (&phase_margin, &gain_margin) {
        (ValueState::Known(pm), ValueState::Known(gm)) => {
            pm.value() >= input.required_phase_margin_deg
                && gm.value() >= input.required_gain_margin_db
        }
        (ValueState::Known(pm), ValueState::Unavailable(_)) => {
            pm.value() >= input.required_phase_margin_deg
        }
        _ => false,
    };
    let compensation_required = !meets_margin;
    let required_noise_gain = if compensation_required {
        let evaluate = |gain: f64| {
            let r_eq = rf * (gain - 1.0) / gain;
            let f_in_at = if c_in > 0.0 && r_eq > 0.0 {
                1.0 / (2.0 * PI * r_eq * c_in)
            } else {
                f64::INFINITY
            };
            let f_fb_at = match feedback_c {
                Some(value) if value > 0.0 => {
                    if r_eq > 0.0 {
                        1.0 / (2.0 * PI * r_eq * value)
                    } else {
                        f_zero
                    }
                }
                _ => f64::INFINITY,
            };
            phase_margin_for_shape(input, fp1, fp2, f_out, f_in_at, f_zero, f_fb_at, gain)
        };
        let mut low = input.noise_gain;
        let mut high = 1_000_000.0;
        match evaluate(high) {
            Some(high_margin) if high_margin >= input.required_phase_margin_deg => {
                for _ in 0..200 {
                    let middle = (low * high).sqrt();
                    match evaluate(middle) {
                        Some(margin) if margin >= input.required_phase_margin_deg => high = middle,
                        Some(_) => low = middle,
                        None => low = middle,
                    }
                }
                Some(high)
            }
            _ => None,
        }
    } else {
        None
    };
    let max_load_capacitance = if compensation_required {
        let evaluate = |total_capacitance: f64| {
            let pole = if total_capacitance > 0.0 {
                1.0 / (2.0 * PI * r_out * total_capacitance)
            } else {
                f64::INFINITY
            };
            let r_eq = rf * (input.noise_gain - 1.0) / input.noise_gain;
            let f_in_at = if c_in > 0.0 && r_eq > 0.0 {
                1.0 / (2.0 * PI * r_eq * c_in)
            } else {
                f64::INFINITY
            };
            let f_fb_at = match feedback_c {
                Some(value) if value > 0.0 => {
                    if r_eq > 0.0 {
                        1.0 / (2.0 * PI * r_eq * value)
                    } else {
                        f_zero
                    }
                }
                _ => f64::INFINITY,
            };
            phase_margin_for_shape(
                input,
                fp1,
                fp2,
                pole,
                f_in_at,
                f_zero,
                f_fb_at,
                input.noise_gain,
            )
        };
        match evaluate(0.0) {
            Some(margin) if margin >= input.required_phase_margin_deg => {
                let mut low = 0.0;
                let mut high = c_total;
                for _ in 0..200 {
                    let middle = (low + high) / 2.0;
                    match evaluate(middle) {
                        Some(margin) if margin >= input.required_phase_margin_deg => low = middle,
                        Some(_) => high = middle,
                        None => high = middle,
                    }
                }
                Some(farad((low - c_out).max(0.0))?)
            }
            _ => None,
        }
    } else {
        None
    };
    let compensation_note = if compensation_required {
        match (&required_noise_gain, &max_load_capacitance) {
            (Some(gain), Some(capacitance)) => format!(
                "the modeled loop misses its margin; raise the noise gain to at least {gain:.3} and/or reduce the load capacitance to at most {:.6} nF, then re-measure the loop",
                capacitance.value() * 1e9
            ),
            (Some(gain), None) => format!(
                "the modeled loop misses its margin; raise the noise gain to at least {gain:.3}, then re-measure the loop"
            ),
            (None, Some(capacitance)) => format!(
                "the modeled loop misses its margin; reduce the load capacitance to at most {:.6} nF, then re-measure the loop",
                capacitance.value() * 1e9
            ),
            (None, None) => "the modeled loop misses its margin and no simple gain/load compensation was found inside the model; change the device or topology".to_string(),
        }
    } else {
        "the modeled phase and gain margins meet the declared acceptance boundary".to_string()
    };

    let mut bcs = BTreeMap::new();
    bcs.insert(
        "first_pole".to_string(),
        boundary_value(&input.first_pole, context)?,
    );
    bcs.insert(
        "second_pole".to_string(),
        boundary_value(&input.second_pole, context)?,
    );
    bcs.insert(
        "output_resistance".to_string(),
        boundary_value(r_out_q, context)?,
    );
    bcs.insert(
        "load_capacitance".to_string(),
        boundary_value(c_load_q, context)?,
    );
    bcs.insert(
        "input_capacitance".to_string(),
        boundary_value(c_in_q, context)?,
    );
    bcs.insert(
        "feedback_resistance".to_string(),
        boundary_value(rf_q, context)?,
    );
    let mut results = vec![
        quantity_result(
            "dc_loop_gain",
            Quantity::dimensionless(input.open_loop_gain * beta0)?,
            Uncertainty::Exact,
        ),
        quantity_result("output_pole", hz(f_out)?, Uncertainty::Exact),
        quantity_result(
            "required_phase_margin",
            deg(input.required_phase_margin_deg)?,
            Uncertainty::Exact,
        ),
    ];
    match &crossover_frequency {
        ValueState::Known(value) => results.push(quantity_result(
            "crossover_frequency",
            value.clone(),
            Uncertainty::Exact,
        )),
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "crossover_frequency",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    match &phase_margin {
        ValueState::Known(value) => results.push(quantity_result(
            "phase_margin",
            value.clone(),
            Uncertainty::Exact,
        )),
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "phase_margin",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    match &gain_margin {
        ValueState::Known(value) => results.push(quantity_result(
            "gain_margin",
            value.clone(),
            Uncertainty::Exact,
        )),
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "gain_margin",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    if let Some(gain) = required_noise_gain {
        results.push(quantity_result(
            "required_noise_gain",
            Quantity::dimensionless(gain)?,
            Uncertainty::Exact,
        ));
    }
    if let Some(capacitance) = &max_load_capacitance {
        results.push(quantity_result(
            "max_stable_load_capacitance",
            capacitance.clone(),
            Uncertainty::Exact,
        ));
    }
    let mut warnings = Vec::new();
    if !stable {
        warnings.push(warning(
            "stability.unstable",
            "the modeled loop is unstable or has non-positive phase margin; the design is rejected and compensation is required",
            WarningSeverity::Critical,
        ));
    } else if compensation_required {
        warnings.push(warning(
            "stability.margin",
            "the loop is stable but does not meet the declared phase/gain margin acceptance boundary",
            WarningSeverity::Warning,
        ));
    }
    let cross_check = crossover.map(|frequency| {
        let (re, im) = complex_response(&shape, frequency);
        let magnitude = re.hypot(im);
        let phase = phase_of_complex(re, im);
        let (analytic_magnitude, analytic_phase) = shape.response(frequency);
        let agreement = CrossCheckEvidence::new(
            "complex-arithmetic loop response at the crossover",
            magnitude,
            1.0,
            1e-6,
        );
        if (phase - analytic_phase).abs() < 1e-9 && (magnitude - analytic_magnitude).abs() < 1e-9 {
            agreement
        } else {
            CrossCheckEvidence::new(
                "complex-arithmetic loop response disagrees with the polar form",
                magnitude,
                analytic_magnitude,
                1e-9,
            )
        }
    });
    let receipt = build_receipt(
        OPAMP_STABILITY_MODEL,
        input,
        MODEL_EXPRESSION,
        &input,
        context,
        bcs,
        Convergence::iterative(4096, 0.0, 1e-9, true),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Solved,
        value: ValueState::known(OpAmpStabilityResult {
            dc_loop_gain: input.open_loop_gain * beta0,
            crossover_frequency,
            phase_margin,
            gain_margin_db: gain_margin,
            output_pole_hz: f_out,
            input_pole_hz: if f_in.is_finite() { Some(f_in) } else { None },
            zero_hz: if f_zero.is_finite() {
                Some(f_zero)
            } else {
                None
            },
            stable,
            meets_margin,
            compensation_required,
            required_noise_gain,
            max_stable_load_capacitance: max_load_capacitance,
            compensation_note,
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
    model_identity(OPAMP_STABILITY_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;
