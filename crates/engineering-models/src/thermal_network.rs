use std::collections::BTreeMap;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ValueState, WarningSeverity,
};
use engineering_units::{Dimension, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_positive, unavailable_result, value_in, warning, AssumptionRecord, CrossCheckEvidence,
    ModelContext, ModelError, ModelReport, ModelResult, ModelStatus, Refusal, RefusalCode,
    MODEL_VERSION,
};

pub const THERMAL_NETWORK_MODEL: &str = "thermal/junction-case-sink-network";
const MODEL_EXPRESSION: &str = "C1 dT1/dt = P - (T1-T2)/R1; C2 dT2/dt = (T1-T2)/R1 - (T2-T3)/R2; C3 dT3/dt = (T2-T3)/R2 - T3/R3; T_ss = T_a + P sum R";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalLayer {
    pub resistance: Quantity,
    pub capacitance: Option<Quantity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalNetworkInput {
    pub power: Quantity,
    pub ambient: Quantity,
    pub junction_to_case: ThermalLayer,
    /// A missing case-to-sink layer is unknown, not zero.
    pub case_to_sink: Option<ThermalLayer>,
    pub sink_to_ambient: ThermalLayer,
    pub transient_times: Vec<Quantity>,
    pub max_junction_temperature: ValueState<Quantity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalTransientPoint {
    pub time: Quantity,
    pub junction: Quantity,
    pub case: Quantity,
    pub sink: Quantity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThermalNetworkResult {
    pub steady_junction: Quantity,
    pub steady_case: Quantity,
    pub steady_sink: Quantity,
    pub thermal_resistance_total: Quantity,
    pub transient: ValueState<Vec<ThermalTransientPoint>>,
    pub transient_peak_junction: ValueState<Quantity>,
    pub thermal_margin: ValueState<Quantity>,
    pub time_constants: Vec<Quantity>,
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
            "asm-lumped-network",
            "The thermal path is a three-node Cauer network with lumped resistance and capacitance per layer; spreading and multi-dimensional effects are outside the model.",
            "Thermal-network knowledge pack: lumped Cauer model.",
        ),
        assumption(
            "asm-constant-properties",
            "Layer resistances and capacitances are constant over the analyzed temperature excursion.",
            "Thermal-network knowledge pack: constant-property validity domain.",
        ),
        assumption(
            "asm-interface-required",
            "The case-to-sink interface layer is a required physical layer; a missing interface resistance makes the network incomplete.",
            "Thermal-network knowledge pack: contact-resistance trap.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "T_ss = T_a + P R_th",
            Dimension::TEMPERATURE,
            Dimension::TEMPERATURE
                .checked_add(Dimension::POWER.checked_mul(Dimension::THERMAL_RESISTANCE)?)?,
        )?,
        DimensionCheck::new(
            "tau = R C",
            Dimension::TIME,
            Dimension::THERMAL_RESISTANCE.checked_mul(Dimension::THERMAL_CAPACITANCE)?,
        )?,
        DimensionCheck::new(
            "dT/dt = P/C",
            Dimension::TEMPERATURE.checked_div(Dimension::TIME)?,
            Dimension::POWER.checked_div(Dimension::THERMAL_CAPACITANCE)?,
        )?,
        DimensionCheck::new(
            "heat flow = dT/R",
            Dimension::POWER,
            Dimension::TEMPERATURE.checked_div(Dimension::THERMAL_RESISTANCE)?,
        )?,
    ])
}

fn kelvin(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("K")?)?)
}

fn kelvin_quantity(quantity: &Quantity) -> ModelResult<f64> {
    require_dimension("temperature", quantity, Dimension::TEMPERATURE)?;
    value_in(quantity, "K")
}

fn celsius_from_k(value: f64) -> ModelResult<Quantity> {
    Ok(kelvin(value)?.to_unit(UnitRegistry::si().require("degC")?)?)
}

fn thermal_resistance(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("K/W")?)?)
}

fn temperature_margin(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(
        value,
        UnitRegistry::si().require("degC_delta")?,
    )?)
}

fn seconds(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("s")?)?)
}

fn mat_mul(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            let mut value = 0.0;
            for k in 0..3 {
                value += a[i][k] * b[k][j];
            }
            out[i][j] = value;
        }
    }
    out
}

fn mat_add(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    let mut out = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            out[i][j] = a[i][j] + b[i][j];
        }
    }
    out
}

fn identity3() -> [[f64; 3]; 3] {
    let mut out = [[0.0; 3]; 3];
    for (index, row) in out.iter_mut().enumerate() {
        row[index] = 1.0;
    }
    out
}

fn matrix_norm(a: &[[f64; 3]; 3]) -> f64 {
    let mut norm: f64 = 0.0;
    for row in a {
        let sum: f64 = row.iter().map(|value| value.abs()).sum();
        norm = norm.max(sum);
    }
    norm
}

fn expm(a: &[[f64; 3]; 3], t: f64) -> [[f64; 3]; 3] {
    let scaled_norm = matrix_norm(a) * t.abs();
    let squarings = if scaled_norm <= 0.5 {
        0
    } else {
        ((scaled_norm / 0.5).log2().ceil() as i32).clamp(0, 80)
    };
    let step = t / 2_f64.powi(squarings);
    let mut x = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            x[i][j] = a[i][j] * step;
        }
    }
    let mut term = identity3();
    let mut result = identity3();
    for order in 1..=14_u32 {
        term = mat_mul(&term, &x);
        let divisor = (1..=order).fold(1.0, |acc, value| acc * value as f64);
        let scaled = {
            let mut value = [[0.0; 3]; 3];
            for i in 0..3 {
                for j in 0..3 {
                    value[i][j] = term[i][j] / divisor;
                }
            }
            value
        };
        result = mat_add(&result, &scaled);
    }
    for _ in 0..squarings {
        result = mat_mul(&result, &result);
    }
    result
}

fn mat_vec(a: &[[f64; 3]; 3], v: &[f64; 3]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for i in 0..3 {
        out[i] = a[i][0] * v[0] + a[i][1] * v[1] + a[i][2] * v[2];
    }
    out
}

fn det3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

fn solve3(m: &[[f64; 3]; 3], rhs: &[f64; 3]) -> Option<[f64; 3]> {
    let determinant = det3(m);
    if determinant.abs() < 1e-18 {
        return None;
    }
    let mut columns = [[[0.0; 3]; 3]; 3];
    for (column, slot) in columns.iter_mut().enumerate() {
        *slot = *m;
        for row in 0..3 {
            slot[row][column] = rhs[row];
        }
    }
    Some([
        det3(&columns[0]) / determinant,
        det3(&columns[1]) / determinant,
        det3(&columns[2]) / determinant,
    ])
}

fn backward_euler(a: &[[f64; 3]; 3], b: &[f64; 3], t: f64, steps: usize) -> Option<[f64; 3]> {
    if steps == 0 || !t.is_finite() || t < 0.0 {
        return None;
    }
    let dt = t / steps as f64;
    let mut m = identity3();
    for i in 0..3 {
        for j in 0..3 {
            m[i][j] -= dt * a[i][j];
        }
    }
    let mut x = [0.0; 3];
    for _ in 0..steps {
        let rhs = [x[0] + dt * b[0], x[1] + dt * b[1], x[2] + dt * b[2]];
        x = solve3(&m, &rhs)?;
    }
    Some(x)
}

fn refusal_report(
    input: &ThermalNetworkInput,
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<ThermalNetworkResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    bcs.insert("power".to_string(), boundary_value(&input.power, context)?);
    bcs.insert(
        "ambient".to_string(),
        boundary_value(&input.ambient, context)?,
    );
    bcs.insert(
        "junction_to_case".to_string(),
        boundary_value(&input.junction_to_case.resistance, context)?,
    );
    if let Some(layer) = &input.case_to_sink {
        bcs.insert(
            "case_to_sink".to_string(),
            boundary_value(&layer.resistance, context)?,
        );
    }
    bcs.insert(
        "sink_to_ambient".to_string(),
        boundary_value(&input.sink_to_ambient.resistance, context)?,
    );
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let results = vec![unavailable_result(
        "steady_junction",
        reason.clone(),
        vec!["a complete thermal path".to_string()],
    )?];
    let receipt = build_receipt(
        THERMAL_NETWORK_MODEL,
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
        value: ValueState::unavailable(reason, vec!["valid thermal network".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

pub fn thermal_network(
    input: &ThermalNetworkInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<ThermalNetworkResult>> {
    let checks = checks()?;
    require_dimension("power", &input.power, Dimension::POWER)?;
    require_dimension("ambient", &input.ambient, Dimension::TEMPERATURE)?;
    let power = value_in(&input.power, "W")?;
    let ambient_k = kelvin_quantity(&input.ambient)?;
    if power < 0.0 {
        return Err(ModelError::InvalidInput {
            field: "power",
            reason: "must be non-negative".to_string(),
        });
    }
    if ambient_k <= 0.0 {
        return Err(ModelError::InvalidInput {
            field: "ambient",
            reason: "absolute temperature must be positive".to_string(),
        });
    }
    let r1 = {
        require_dimension(
            "junction_to_case.resistance",
            &input.junction_to_case.resistance,
            Dimension::THERMAL_RESISTANCE,
        )?;
        let value = value_in(&input.junction_to_case.resistance, "K/W")?;
        require_positive("junction_to_case.resistance", value)?;
        value
    };
    let Some(case_layer) = &input.case_to_sink else {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::MissingInterfaceResistance,
                "case-to-sink interface resistance is not established; missing interface resistance is unknown, not zero",
                vec![
                    "revision-bound thermal interface material data at the assembly pressure"
                        .to_string(),
                ],
            ),
            checks,
        );
    };
    let r2 = {
        require_dimension(
            "case_to_sink.resistance",
            &case_layer.resistance,
            Dimension::THERMAL_RESISTANCE,
        )?;
        let value = value_in(&case_layer.resistance, "K/W")?;
        require_positive("case_to_sink.resistance", value)?;
        value
    };
    let r3 = {
        require_dimension(
            "sink_to_ambient.resistance",
            &input.sink_to_ambient.resistance,
            Dimension::THERMAL_RESISTANCE,
        )?;
        let value = value_in(&input.sink_to_ambient.resistance, "K/W")?;
        require_positive("sink_to_ambient.resistance", value)?;
        value
    };
    let total_r = r1 + r2 + r3;
    let steady_j = ambient_k + power * total_r;
    let steady_c = ambient_k + power * (r2 + r3);
    let steady_s = ambient_k + power * r3;
    if input.transient_times.len() > 64 {
        return Err(ModelError::InvalidInput {
            field: "transient_times",
            reason: "at most 64 transient sample times are supported".to_string(),
        });
    }
    let mut transient_times = Vec::with_capacity(input.transient_times.len());
    for time in &input.transient_times {
        require_dimension("transient_times", time, Dimension::TIME)?;
        let value = value_in(time, "s")?;
        if value < 0.0 || value > 1e9 {
            return Err(ModelError::InvalidInput {
                field: "transient_times",
                reason: "times must be in [0, 1e9] s".to_string(),
            });
        }
        if transient_times.last().is_some_and(|last| value < *last) {
            return Err(ModelError::InvalidInput {
                field: "transient_times",
                reason: "times must be non-decreasing".to_string(),
            });
        }
        transient_times.push(value);
    }
    let capacitance = |name: &'static str, layer: &ThermalLayer| -> ModelResult<f64> {
        match &layer.capacitance {
            Some(quantity) => {
                require_dimension(name, quantity, Dimension::THERMAL_CAPACITANCE)?;
                let value = value_in(quantity, "J/K")?;
                require_positive(name, value)?;
                Ok(value)
            }
            None => Err(ModelError::InvalidInput {
                field: name,
                reason: "thermal capacitance is required for transient analysis".to_string(),
            }),
        }
    };
    let mut time_constants = Vec::new();
    let transient = if transient_times.is_empty() {
        ValueState::unavailable(
            "no transient sample times were requested; only the steady-state network is reported",
            vec!["at least one transient sample time".to_string()],
        )?
    } else {
        match (
            &input.junction_to_case.capacitance,
            &case_layer.capacitance,
            &input.sink_to_ambient.capacitance,
        ) {
            (Some(_), Some(_), Some(_)) => {
                let c1 = capacitance("junction_to_case.capacitance", &input.junction_to_case)?;
                let c2 = capacitance("case_to_sink.capacitance", case_layer)?;
                let c3 = capacitance(
                    "sink_to_ambient.capacitance",
                    &input.sink_to_ambient,
                )?;
                let mut a = [[0.0; 3]; 3];
                a[0][0] = -1.0 / (r1 * c1);
                a[0][1] = 1.0 / (r1 * c1);
                a[1][0] = 1.0 / (r1 * c2);
                a[1][1] = -1.0 / (r1 * c2) - 1.0 / (r2 * c2);
                a[1][2] = 1.0 / (r2 * c2);
                a[2][1] = 1.0 / (r2 * c3);
                a[2][2] = -1.0 / (r2 * c3) - 1.0 / (r3 * c3);
                let steady_relative = [steady_j - ambient_k, steady_c - ambient_k, steady_s - ambient_k];
                let mut points = Vec::with_capacity(transient_times.len());
                for time in &transient_times {
                    let transition = expm(&a, *time);
                    let initial = [-steady_relative[0], -steady_relative[1], -steady_relative[2]];
                    let delta = mat_vec(&transition, &initial);
                    let x = [
                        steady_relative[0] + delta[0],
                        steady_relative[1] + delta[1],
                        steady_relative[2] + delta[2],
                    ];
                    points.push(ThermalTransientPoint {
                        time: seconds(*time)?,
                        junction: celsius_from_k(ambient_k + x[0])?,
                        case: celsius_from_k(ambient_k + x[1])?,
                        sink: celsius_from_k(ambient_k + x[2])?,
                    });
                }
                time_constants.extend([
                    seconds(r1 * c1)?,
                    seconds(r2 * c2)?,
                    seconds(r3 * c3)?,
                ]);
                ValueState::Known(points)
            }
            _ => ValueState::unavailable(
                "one or more layer thermal capacitances are unavailable; the transient RC ladder cannot be evaluated",
                vec![
                    "junction-to-case thermal capacitance".to_string(),
                    "case-to-sink thermal capacitance".to_string(),
                    "sink-to-ambient thermal capacitance".to_string(),
                ],
            )?,
        }
    };
    let transient_peak_junction = match &transient {
        ValueState::Known(points) => {
            let peak = points
                .iter()
                .map(|point| kelvin_quantity(&point.junction))
                .collect::<ModelResult<Vec<_>>>()?
                .into_iter()
                .fold(ambient_k, f64::max)
                .max(steady_j);
            ValueState::Known(celsius_from_k(peak)?)
        }
        ValueState::Unavailable(reason) => ValueState::Unavailable(reason.clone()),
    };
    let thermal_margin = match &input.max_junction_temperature {
        ValueState::Known(limit) => {
            let limit_k = kelvin_quantity(limit)?;
            ValueState::Known(temperature_margin(limit_k - steady_j)?)
        }
        ValueState::Unavailable(reason) => ValueState::Unavailable(reason.clone()),
    };
    let mut warnings = Vec::new();
    if !transient.is_known() {
        warnings.push(warning(
            "transient.unavailable",
            "transient response is unavailable because the layer capacitances are incomplete",
            WarningSeverity::Warning,
        ));
    }
    if !input.max_junction_temperature.is_known() {
        warnings.push(warning(
            "margin.unavailable",
            "maximum junction temperature is unavailable; the thermal margin is not established",
            WarningSeverity::Warning,
        ));
    }
    let mut bcs = BTreeMap::new();
    bcs.insert("power".to_string(), boundary_value(&input.power, context)?);
    bcs.insert(
        "ambient".to_string(),
        boundary_value(&input.ambient, context)?,
    );
    bcs.insert(
        "junction_to_case".to_string(),
        boundary_value(&input.junction_to_case.resistance, context)?,
    );
    bcs.insert(
        "case_to_sink".to_string(),
        boundary_value(&case_layer.resistance, context)?,
    );
    bcs.insert(
        "sink_to_ambient".to_string(),
        boundary_value(&input.sink_to_ambient.resistance, context)?,
    );
    let mut results = vec![
        quantity_result(
            "steady_junction",
            celsius_from_k(steady_j)?,
            Uncertainty::Exact,
        ),
        quantity_result("steady_case", celsius_from_k(steady_c)?, Uncertainty::Exact),
        quantity_result("steady_sink", celsius_from_k(steady_s)?, Uncertainty::Exact),
        quantity_result(
            "thermal_resistance_total",
            thermal_resistance(total_r)?,
            Uncertainty::Exact,
        ),
    ];
    match &transient_peak_junction {
        ValueState::Known(value) => results.push(quantity_result(
            "transient_peak_junction",
            value.clone(),
            Uncertainty::Exact,
        )),
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "transient_peak_junction",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    match &thermal_margin {
        ValueState::Known(value) => results.push(quantity_result(
            "thermal_margin",
            value.clone(),
            Uncertainty::Exact,
        )),
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "thermal_margin",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    if !time_constants.is_empty() {
        results.push(quantity_result(
            "junction_case_time_constant",
            time_constants[0].clone(),
            Uncertainty::Exact,
        ));
    }
    let cross_check = match (
        &transient,
        transient_times.iter().copied().fold(0.0, f64::max),
    ) {
        (ValueState::Known(points), last) if last > 0.0 => {
            let c1 = capacitance("junction_to_case.capacitance", &input.junction_to_case)?;
            let c2 = capacitance("case_to_sink.capacitance", case_layer)?;
            let c3 = capacitance("sink_to_ambient.capacitance", &input.sink_to_ambient)?;
            let mut a = [[0.0; 3]; 3];
            a[0][0] = -1.0 / (r1 * c1);
            a[0][1] = 1.0 / (r1 * c1);
            a[1][0] = 1.0 / (r1 * c2);
            a[1][1] = -1.0 / (r1 * c2) - 1.0 / (r2 * c2);
            a[1][2] = 1.0 / (r2 * c2);
            a[2][1] = 1.0 / (r2 * c3);
            a[2][2] = -1.0 / (r2 * c3) - 1.0 / (r3 * c3);
            let b = [power / c1, 0.0, 0.0];
            let numerical = backward_euler(&a, &b, last, 4096);
            let analytical = points
                .last()
                .map(|point| kelvin_quantity(&point.junction))
                .transpose()?;
            match (numerical, analytical) {
                (Some(x), Some(expected_k)) => {
                    let actual_k = ambient_k + x[0];
                    Some(CrossCheckEvidence::new(
                        "backward-Euler integration of the thermal ladder",
                        actual_k,
                        expected_k,
                        1e-3 * expected_k.max(1.0),
                    ))
                }
                _ => Some(CrossCheckEvidence::new(
                    "backward-Euler integration could not be completed",
                    0.0,
                    1.0,
                    0.0,
                )),
            }
        }
        _ => None,
    };
    let receipt = build_receipt(
        THERMAL_NETWORK_MODEL,
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
        value: ValueState::known(ThermalNetworkResult {
            steady_junction: celsius_from_k(steady_j)?,
            steady_case: celsius_from_k(steady_c)?,
            steady_sink: celsius_from_k(steady_s)?,
            thermal_resistance_total: thermal_resistance(total_r)?,
            transient,
            transient_peak_junction,
            thermal_margin,
            time_constants,
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
    model_identity(THERMAL_NETWORK_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;
