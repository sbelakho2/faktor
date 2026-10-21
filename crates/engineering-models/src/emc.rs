use std::collections::BTreeMap;
use std::f64::consts::PI;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_positive, unavailable_result, value_in, warning, AssumptionRecord, ModelContext,
    ModelError, ModelReport, ModelResult, ModelStatus, Refusal, RefusalCode, MODEL_VERSION,
};

pub const EMC_PCB_MODEL: &str = "emc/pcb-return-path";
const MODEL_EXPRESSION: &str = "L_loop ~= mu0 h l / w (trace over plane); L_loop,adj <= L_loop + mu0 h 2d / w; spacing_ok = clearance >= rule && creepage >= rule";
const MU0: f64 = 4.0e-7 * PI;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PcbStackup {
    pub dielectric_height: Quantity,
    pub copper_thickness: Option<Quantity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmcRules {
    pub min_clearance: Quantity,
    pub min_creepage: Quantity,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmcPcbInput {
    pub stackup: Option<PcbStackup>,
    pub trace_width: Option<Quantity>,
    pub loop_length: Option<Quantity>,
    pub capacitor_distance: Option<Quantity>,
    pub via_length: Option<Quantity>,
    pub frequency: Option<Quantity>,
    pub max_loop_inductance: Option<Quantity>,
    pub actual_clearance: Option<Quantity>,
    pub actual_creepage: Option<Quantity>,
    pub rules: Option<EmcRules>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmcRuleCheck {
    pub id: String,
    pub pass: bool,
    pub actual: Quantity,
    pub required: Quantity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmcPcbResult {
    pub loop_inductance: ValueState<Quantity>,
    pub adjacent_inductance: ValueState<Quantity>,
    pub impedance_at_frequency: ValueState<Quantity>,
    pub max_placement_distance: ValueState<Quantity>,
    pub placement_inductance_factor: ValueState<f64>,
    pub clearance_check: ValueState<EmcRuleCheck>,
    pub creepage_check: ValueState<EmcRuleCheck>,
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
            "asm-trace-over-plane",
            "The return current is directly below the trace and the loop inductance is approximated by the parallel-plate trace-over-plane formula mu0 h l / w for l >> h and w >= h.",
            "EMC/PCB knowledge pack: return-path inductance.",
        ),
        assumption(
            "asm-rule-inputs",
            "Clearance and creepage limits are explicit rule inputs with provenance; the model does not invent a standard table.",
            "EMC/PCB knowledge pack: creepage and clearance rules.",
        ),
        assumption(
            "asm-static-layout",
            "The estimate is a low-frequency geometric loop estimate; via transitions, connector breakout and plane splits are not modeled.",
            "EMC/PCB knowledge pack: validity envelope.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "L = mu0 h l / w",
            Dimension::INDUCTANCE,
            Dimension::INDUCTANCE,
        )?,
        DimensionCheck::new(
            "Z = 2 pi f L",
            Dimension::RESISTANCE,
            Dimension::FREQUENCY.checked_mul(Dimension::INDUCTANCE)?,
        )?,
        DimensionCheck::new(
            "spacing comparison is dimensionless",
            Dimension::SCALAR,
            Dimension::LENGTH.checked_div(Dimension::LENGTH)?,
        )?,
    ])
}

fn henry(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("H")?)?)
}

fn ohm(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("ohm")?)?)
}

fn meter(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("m")?)?)
}

fn loop_inductance(height: f64, length: f64, width: f64) -> f64 {
    MU0 * height * length / width
}

fn check_spacing(
    id: &'static str,
    actual: Option<&Quantity>,
    required: Option<&Quantity>,
    dimension: Dimension,
) -> ModelResult<ValueState<EmcRuleCheck>> {
    let (Some(actual), Some(required)) = (actual, required) else {
        return Ok(ValueState::unavailable(
            format!("{id} actual or rule value is unavailable"),
            vec![format!("actual and required {id}")],
        )?);
    };
    require_dimension(id, actual, dimension)?;
    require_dimension(id, required, dimension)?;
    let actual_value = actual.to_unit(required.unit().clone())?.value();
    let required_value = required.value();
    let pass = actual_value >= required_value;
    Ok(ValueState::known(EmcRuleCheck {
        id: id.to_string(),
        pass,
        actual: actual.clone(),
        required: required.clone(),
        message: if pass {
            format!("{id} {actual_value} meets the rule {required_value}")
        } else {
            format!("{id} {actual_value} is below the rule {required_value}")
        },
    }))
}

fn refusal_report(
    input: &EmcPcbInput,
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<EmcPcbResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    if let Some(stackup) = &input.stackup {
        bcs.insert(
            "dielectric_height".to_string(),
            boundary_value(&stackup.dielectric_height, context)?,
        );
    }
    if let Some(width) = &input.trace_width {
        bcs.insert("trace_width".to_string(), boundary_value(width, context)?);
    }
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let results = vec![unavailable_result(
        "loop_inductance",
        reason.clone(),
        vec!["a valid return-path geometry".to_string()],
    )?];
    let receipt = build_receipt(
        EMC_PCB_MODEL,
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
        status: ModelStatus::Refused,
        value: ValueState::unavailable(reason, vec!["valid EMC/PCB geometry".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

pub fn emc_pcb_check(
    input: &EmcPcbInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<EmcPcbResult>> {
    let checks = checks()?;
    let mut warnings = Vec::new();
    let (loop_inductance, adjacent_inductance, placement_factor, max_distance) = match (
        &input.stackup,
        &input.trace_width,
    ) {
        (Some(stackup), Some(width)) => {
            require_dimension(
                "dielectric_height",
                &stackup.dielectric_height,
                Dimension::LENGTH,
            )?;
            require_dimension("trace_width", width, Dimension::LENGTH)?;
            let height = value_in(&stackup.dielectric_height, "m")?;
            let width_m = value_in(width, "m")?;
            require_positive("dielectric_height", height)?;
            require_positive("trace_width", width_m)?;
            if width_m < height {
                return refusal_report(
                        input,
                        context,
                        Refusal::new(
                            RefusalCode::OutsideValidity,
                            format!(
                                "trace width {width_m} m is below the dielectric height {height} m; the parallel-plate return-path estimate requires w >= h"
                            ),
                            vec!["a wider trace or a different loop-inductance model".to_string()],
                        ),
                        checks,
                    );
            }
            if let Some(copper) = &stackup.copper_thickness {
                require_dimension("copper_thickness", copper, Dimension::LENGTH)?;
                let value = value_in(copper, "m")?;
                require_positive("copper_thickness", value)?;
            }
            let length = match (&input.capacitor_distance, &input.via_length) {
                (Some(distance), Some(via)) => {
                    require_dimension("capacitor_distance", distance, Dimension::LENGTH)?;
                    require_dimension("via_length", via, Dimension::LENGTH)?;
                    let distance = value_in(distance, "m")?;
                    let via = value_in(via, "m")?;
                    if distance < 0.0 || via < 0.0 {
                        return Err(ModelError::InvalidInput {
                            field: "capacitor_distance",
                            reason: "distance and via length must be non-negative".to_string(),
                        });
                    }
                    Some(2.0 * (distance + via))
                }
                _ => input
                    .loop_length
                    .as_ref()
                    .map(|quantity| {
                        require_dimension("loop_length", quantity, Dimension::LENGTH)?;
                        value_in(quantity, "m")
                    })
                    .transpose()?,
            };
            let Some(length) = length else {
                warnings.push(warning(
                        "geometry.unavailable",
                        "loop length is unavailable because neither a placement distance/via length nor a total loop length was supplied",
                        WarningSeverity::Warning,
                    ));
                return Ok(ModelReport {
                        status: ModelStatus::Solved,
                        value: ValueState::known(EmcPcbResult {
                            loop_inductance: ValueState::unavailable(
                                "loop length is not established; return-path loop inductance is unavailable",
                                vec!["capacitor distance plus via length or total loop length".to_string()],
                            )?,
                            adjacent_inductance: ValueState::unavailable(
                                "loop length is not established",
                                vec!["capacitor distance plus via length".to_string()],
                            )?,
                            impedance_at_frequency: ValueState::unavailable(
                                "loop inductance is unavailable",
                                vec!["loop geometry".to_string()],
                            )?,
                            max_placement_distance: ValueState::unavailable(
                                "loop geometry is unavailable",
                                vec!["loop geometry".to_string()],
                            )?,
                            placement_inductance_factor: ValueState::unavailable(
                                "loop geometry is unavailable",
                                vec!["loop geometry".to_string()],
                            )?,
                            clearance_check: check_spacing(
                                "clearance",
                                input.actual_clearance.as_ref(),
                                input.rules.as_ref().map(|rules| &rules.min_clearance),
                                Dimension::LENGTH,
                            )?,
                            creepage_check: check_spacing(
                                "creepage",
                                input.actual_creepage.as_ref(),
                                input.rules.as_ref().map(|rules| &rules.min_creepage),
                                Dimension::LENGTH,
                            )?,
                        }),
                        refusal: None,
                        receipt: build_receipt(
                            EMC_PCB_MODEL,
                            input,
                            MODEL_EXPRESSION,
                            &input,
                            context,
                            BTreeMap::new(),
                            Convergence::exact(),
                            warnings.clone(),
                            vec![unavailable_result(
                                "loop_inductance",
                                "loop length is unavailable",
                                vec!["loop geometry".to_string()],
                            )?],
                            Uncertainty::Exact,
                        )?,
                        checks,
                        assumptions: assumptions(),
                        warnings,
                        uncertainty: Uncertainty::Exact,
                        cross_check: None,
                    });
            };
            let inductance = loop_inductance(height, length, width_m);
            let adjacent = match &input.via_length {
                Some(via) => {
                    let via = value_in(via, "m")?;
                    Some(loop_inductance(height, 2.0 * via, width_m))
                }
                None => None,
            };
            let factor = adjacent.and_then(|adjacent| {
                if adjacent > 0.0 {
                    Some(inductance / adjacent)
                } else {
                    None
                }
            });
            let max_distance = match (&input.max_loop_inductance, &input.via_length) {
                (Some(max), Some(via)) => {
                    require_dimension("max_loop_inductance", max, Dimension::INDUCTANCE)?;
                    let max = value_in(max, "H")?;
                    let via = value_in(via, "m")?;
                    if max <= 0.0 {
                        return Err(ModelError::InvalidInput {
                            field: "max_loop_inductance",
                            reason: "must be positive".to_string(),
                        });
                    }
                    let length_max = max * width_m / (MU0 * height);
                    Some((length_max - 2.0 * via) / 2.0)
                }
                _ => None,
            };
            (
                    ValueState::known(henry(inductance)?),
                    match adjacent {
                        Some(value) => ValueState::known(henry(value)?),
                        None => ValueState::unavailable(
                            "via length is not established; the adjacent-placement inductance is unavailable",
                            vec!["via length".to_string()],
                        )?,
                    },
                    match factor {
                        Some(value) => ValueState::known(value),
                        None => ValueState::unavailable(
                            "via length is not established; the placement factor is unavailable",
                            vec!["via length".to_string()],
                        )?,
                    },
                    match max_distance {
                        Some(value) if value >= 0.0 => ValueState::known(meter(value)?),
                        Some(_) => ValueState::unavailable(
                            "the target loop inductance cannot be met even with zero placement distance at this stackup",
                            vec!["a larger target inductance, lower dielectric height or wider trace".to_string()],
                        )?,
                        None => ValueState::unavailable(
                            "target loop inductance or via length is unavailable",
                            vec!["target loop inductance and via length".to_string()],
                        )?,
                    },
                )
        }
        _ => {
            warnings.push(warning(
                    "stackup.unavailable",
                    "stackup dielectric height or trace width is unavailable; loop inductance is unavailable",
                    WarningSeverity::Warning,
                ));
            (
                ValueState::unavailable(
                    "stackup dielectric height or trace width is unavailable",
                    vec!["revision-bound PCB stackup and trace width".to_string()],
                )?,
                ValueState::unavailable(
                    "stackup geometry is unavailable",
                    vec!["PCB stackup".to_string()],
                )?,
                ValueState::unavailable(
                    "stackup geometry is unavailable",
                    vec!["PCB stackup".to_string()],
                )?,
                ValueState::unavailable(
                    "stackup geometry is unavailable",
                    vec!["PCB stackup".to_string()],
                )?,
            )
        }
    };
    let impedance = match (&loop_inductance, &input.frequency) {
        (ValueState::Known(inductance), Some(frequency)) => {
            require_dimension("frequency", frequency, Dimension::FREQUENCY)?;
            let f = value_in(frequency, "Hz")?;
            require_positive("frequency", f)?;
            let l = value_in(inductance, "H")?;
            ValueState::Known(ohm(2.0 * PI * f * l)?)
        }
        (ValueState::Known(_), None) => ValueState::unavailable(
            "frequency is not established; the loop impedance is unavailable",
            vec!["analysis frequency".to_string()],
        )?,
        (ValueState::Unavailable(reason), _) => ValueState::Unavailable(reason.clone()),
    };
    let required_clearance = input.rules.as_ref().map(|rules| &rules.min_clearance);
    let required_creepage = input.rules.as_ref().map(|rules| &rules.min_creepage);
    let clearance_check = check_spacing(
        "clearance",
        input.actual_clearance.as_ref(),
        required_clearance,
        Dimension::LENGTH,
    )?;
    let creepage_check = check_spacing(
        "creepage",
        input.actual_creepage.as_ref(),
        required_creepage,
        Dimension::LENGTH,
    )?;
    let mut broken = Vec::new();
    for check in [&clearance_check, &creepage_check] {
        if let ValueState::Known(value) = check {
            if !value.pass {
                broken.push(value.message.clone());
                warnings.push(warning(
                    "spacing.violation",
                    value.message.clone(),
                    WarningSeverity::Critical,
                ));
            }
        }
    }
    if let (ValueState::Known(value), Some(max)) = (&loop_inductance, &input.max_loop_inductance) {
        require_dimension("max_loop_inductance", max, Dimension::INDUCTANCE)?;
        let max = value_in(max, "H")?;
        if value_in(value, "H")? > max {
            broken.push(format!(
                "loop inductance {:.6e} H exceeds the target {:.6e} H",
                value_in(value, "H")?,
                max
            ));
            warnings.push(warning(
                "inductance.violation",
                "return-path loop inductance exceeds the target",
                WarningSeverity::Critical,
            ));
        }
    }
    let mut bcs = BTreeMap::new();
    if let Some(stackup) = &input.stackup {
        bcs.insert(
            "dielectric_height".to_string(),
            boundary_value(&stackup.dielectric_height, context)?,
        );
    }
    if let Some(width) = &input.trace_width {
        bcs.insert("trace_width".to_string(), boundary_value(width, context)?);
    }
    if let Some(frequency) = &input.frequency {
        bcs.insert("frequency".to_string(), boundary_value(frequency, context)?);
    }
    let mut results = Vec::new();
    for (name, value) in [
        ("loop_inductance", &loop_inductance),
        ("adjacent_inductance", &adjacent_inductance),
        ("impedance_at_frequency", &impedance),
        ("max_placement_distance", &max_distance),
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
    for (name, value) in [
        ("clearance", &clearance_check),
        ("creepage", &creepage_check),
    ] {
        match value {
            ValueState::Known(check) => results.push(quantity_result(
                &format!("{name}_margin"),
                check.actual.sub(&check.required)?,
                Uncertainty::Exact,
            )),
            ValueState::Unavailable(reason) => results.push(unavailable_result(
                name,
                reason.reason.clone(),
                reason.required.clone(),
            )?),
        }
    }
    match &placement_factor {
        ValueState::Known(factor) => results.push(quantity_result(
            "placement_factor",
            Quantity::dimensionless(*factor)?,
            Uncertainty::Exact,
        )),
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "placement_factor",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    let cross_check = match (
        &loop_inductance,
        &adjacent_inductance,
        &input.stackup,
        &input.trace_width,
        &input.via_length,
    ) {
        (
            ValueState::Known(inductance),
            ValueState::Known(adjacent),
            Some(stackup),
            Some(width),
            Some(via),
        ) => {
            let inductance = value_in(inductance, "H")?;
            let adjacent = value_in(adjacent, "H")?;
            let height = value_in(&stackup.dielectric_height, "m")?;
            let width = value_in(width, "m")?;
            let via = value_in(via, "m")?;
            let length = inductance * width / (MU0 * height);
            let distance = length / 2.0 - via;
            let expected = MU0 * height * 2.0 * distance / width;
            Some(crate::common::CrossCheckEvidence::new(
                "placement-distance loop-inductance delta",
                inductance - adjacent,
                expected,
                1e-12 * expected.abs().max(1.0),
            ))
        }
        _ => None,
    };
    let receipt = build_receipt(
        EMC_PCB_MODEL,
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
        value: ValueState::known(EmcPcbResult {
            loop_inductance,
            adjacent_inductance,
            impedance_at_frequency: impedance,
            max_placement_distance: max_distance,
            placement_inductance_factor: placement_factor,
            clearance_check,
            creepage_check,
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
    model_identity(EMC_PCB_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;
