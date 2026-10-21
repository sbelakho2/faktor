use std::collections::BTreeMap;

use engineering_core::{
    Convergence, DimensionCheck, Provenance, ProvenanceKind, ValueState, WarningSeverity,
};
use engineering_units::{sample_std_dev, Dimension, Interval, Quantity, Uncertainty, UnitRegistry};
use serde::{Deserialize, Serialize};

use crate::common::{
    boundary_value, build_receipt, model_identity, quantity_result, require_dimension,
    require_positive, unavailable_result, value_in, warning, AssumptionRecord, CrossCheckEvidence,
    ModelContext, ModelError, ModelReport, ModelResult, ModelStatus, QuantityRange, Refusal,
    RefusalCode, MODEL_VERSION,
};

pub const BOLTED_JOINT_MODEL: &str = "bolted-joint/torque-preload";
const MODEL_EXPRESSION: &str = "T = K F d; A_t = pi/4 (d - 0.9382 p)^2; sigma = F / A_t";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NutFactorScatter {
    /// Point value used for the nominal estimate. It must lie inside the
    /// declared validity interval.
    pub nominal: f64,
    pub uncertainty: Uncertainty,
    /// The domain in which the K value is qualified. A value outside this
    /// interval is refused, not clamped.
    pub validity: Interval,
}

impl NutFactorScatter {
    pub fn new(nominal: f64, uncertainty: Uncertainty, validity: Interval) -> ModelResult<Self> {
        require_positive("nut_factor.nominal", nominal)?;
        if validity.lower() <= 0.0 {
            return Err(ModelError::InvalidInput {
                field: "nut_factor.validity",
                reason: "nut factor must be positive across its validity interval".to_string(),
            });
        }
        if !validity.contains(nominal) {
            return Err(ModelError::InvalidInput {
                field: "nut_factor.nominal",
                reason: format!(
                    "nominal {nominal} lies outside validity [{}, {}]",
                    validity.lower(),
                    validity.upper()
                ),
            });
        }
        Ok(Self {
            nominal,
            uncertainty,
            validity,
        })
    }

    pub fn bounds(&self) -> Interval {
        self.uncertainty
            .interval()
            .unwrap_or_else(|| Interval::point(self.nominal).unwrap_or(self.validity))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorquePreloadInput {
    pub torque: Quantity,
    pub nominal_diameter: Quantity,
    /// Thread pitch is needed for the nominal tensile stress area unless an
    /// explicit revision-bound area is supplied.
    pub thread_pitch: Option<Quantity>,
    pub tensile_area: Option<Quantity>,
    pub nut_factor: NutFactorScatter,
    /// A missing proof strength leaves utilization unavailable, never zero.
    pub proof_strength: ValueState<Quantity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TorquePreloadResult {
    pub preload_nominal: Quantity,
    pub preload_bounds: Option<QuantityRange>,
    pub preload_std_dev: Option<Quantity>,
    pub nut_factor_bounds: Interval,
    pub tensile_area: ValueState<Quantity>,
    pub bolt_stress: ValueState<Quantity>,
    pub utilization: ValueState<f64>,
}

fn newton(value: f64) -> ModelResult<Quantity> {
    Ok(Quantity::new(value, UnitRegistry::si().require("N")?)?)
}

fn assumption(id: &str, statement: &str, citation: &str) -> AssumptionRecord {
    AssumptionRecord::new(
        id,
        statement,
        Provenance::new(ProvenanceKind::EngineeringPractice, citation).unwrap_or_else(|_| {
            Provenance::new(ProvenanceKind::EngineeringPractice, "engineering practice")
                .expect("non-empty static provenance")
        }),
    )
}

fn assumptions() -> Vec<AssumptionRecord> {
    vec![
        assumption(
            "asm-k-lumps-friction",
            "Thread and underhead friction are lumped into the condition-specific nut factor K.",
            "Bolted-joint knowledge pack: friction is not a material constant.",
        ),
        assumption(
            "asm-quasi-static",
            "The tightening torque is quasi-static and all of it reaches the fastener.",
            "ISO 16047 test-condition practice.",
        ),
        assumption(
            "asm-no-relaxation",
            "Embedment, creep and relaxation after tightening are outside this scalar model.",
            "VDI 2230 joint behaviour assumptions.",
        ),
    ]
}

fn checks() -> ModelResult<Vec<DimensionCheck>> {
    Ok(vec![
        DimensionCheck::new(
            "T = K F d",
            Dimension::ENERGY,
            Dimension::SCALAR
                .checked_mul(Dimension::FORCE)?
                .checked_mul(Dimension::LENGTH)?,
        )?,
        DimensionCheck::new(
            "F = T / (K d)",
            Dimension::FORCE,
            Dimension::ENERGY
                .checked_div(Dimension::SCALAR)?
                .checked_div(Dimension::LENGTH)?,
        )?,
        DimensionCheck::new(
            "sigma = F / A_t",
            Dimension::PRESSURE,
            Dimension::FORCE.checked_div(Dimension::AREA)?,
        )?,
        DimensionCheck::new(
            "A_t = pi/4 (d - 0.9382 p)^2",
            Dimension::AREA,
            Dimension::LENGTH.checked_powi(2)?,
        )?,
    ])
}

fn refusal_report(
    input: &TorquePreloadInput,
    context: &ModelContext,
    refusal: Refusal,
    checks: Vec<DimensionCheck>,
) -> ModelResult<ModelReport<TorquePreloadResult>> {
    let reason = refusal.reason.clone();
    let mut bcs = BTreeMap::new();
    bcs.insert(
        "torque".to_string(),
        boundary_value(&input.torque, context)?,
    );
    bcs.insert(
        "nominal_diameter".to_string(),
        boundary_value(&input.nominal_diameter, context)?,
    );
    bcs.insert(
        "nut_factor".to_string(),
        boundary_value(
            &Quantity::new(input.nut_factor.nominal, UnitRegistry::si().require("1")?)?,
            context,
        )?,
    );
    let results = vec![unavailable_result(
        "preload_nominal",
        reason.clone(),
        vec!["nut-factor validity range".to_string()],
    )?];
    let warnings = vec![warning(
        "model.refused",
        reason.clone(),
        WarningSeverity::Critical,
    )];
    let receipt = build_receipt(
        BOLTED_JOINT_MODEL,
        input,
        MODEL_EXPRESSION,
        &input.nut_factor,
        context,
        bcs,
        Convergence::iterative(1, 0.0, 1.0, false),
        warnings.clone(),
        results,
        Uncertainty::Exact,
    )?;
    Ok(ModelReport {
        status: ModelStatus::Refused,
        value: ValueState::unavailable(reason, vec!["valid nut factor".to_string()])?,
        refusal: Some(refusal),
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: Uncertainty::Exact,
        cross_check: None,
    })
}

pub fn torque_preload(
    input: &TorquePreloadInput,
    context: &ModelContext,
) -> ModelResult<ModelReport<TorquePreloadResult>> {
    require_dimension("torque", &input.torque, Dimension::ENERGY)?;
    require_dimension(
        "nominal_diameter",
        &input.nominal_diameter,
        Dimension::LENGTH,
    )?;
    let checks = checks()?;

    require_positive("torque", value_in(&input.torque, "N*m")?)?;
    let diameter_m = value_in(&input.nominal_diameter, "m")?;
    require_positive("nominal_diameter", diameter_m)?;
    let torque_nm = value_in(&input.torque, "N*m")?;

    let k_bounds = input.nut_factor.bounds();
    if k_bounds.lower() <= 0.0
        || !input.nut_factor.validity.contains(k_bounds.lower())
        || !input.nut_factor.validity.contains(k_bounds.upper())
    {
        return refusal_report(
            input,
            context,
            Refusal::new(
                RefusalCode::OutsideValidity,
                format!(
                    "nut factor scatter [{}, {}] is outside the qualified validity range [{}, {}]",
                    k_bounds.lower(),
                    k_bounds.upper(),
                    input.nut_factor.validity.lower(),
                    input.nut_factor.validity.upper()
                ),
                vec!["nut-factor validity range from the joint-specific test".to_string()],
            ),
            checks,
        );
    }

    let k_nominal = input.nut_factor.nominal;
    let preload_n = torque_nm / (k_nominal * diameter_m);
    require_positive("preload", preload_n)?;
    let preload_min = torque_nm / (k_bounds.upper() * diameter_m);
    let preload_max = torque_nm / (k_bounds.lower() * diameter_m);
    let preload_bounds = QuantityRange::new(newton(preload_min)?, newton(preload_max)?)?;

    let preload_std_dev = match input.nut_factor.uncertainty {
        Uncertainty::Exact | Uncertainty::Interval(_) => None,
        Uncertainty::Distribution(distribution) => {
            let samples = distribution.stratified_samples(2_048)?;
            let loads: Vec<f64> = samples
                .into_iter()
                .map(|k| torque_nm / (k * diameter_m))
                .collect();
            let std = sample_std_dev(&loads)?;
            Some(newton(std)?)
        }
    };

    let tensile_area = if let Some(area) = &input.tensile_area {
        require_dimension("tensile_area", area, Dimension::AREA)?;
        let area_m2 = value_in(area, "m^2")?;
        require_positive("tensile_area", area_m2)?;
        ValueState::known(Quantity::new(area_m2, UnitRegistry::si().require("m^2")?)?)
    } else if let Some(pitch) = &input.thread_pitch {
        require_dimension("thread_pitch", pitch, Dimension::LENGTH)?;
        let pitch_m = value_in(pitch, "m")?;
        require_positive("thread_pitch", pitch_m)?;
        let effective = diameter_m - 0.9382 * pitch_m;
        if effective <= 0.0 {
            return Err(ModelError::InvalidInput {
                field: "thread_pitch",
                reason: "pitch produces a non-positive tensile-stress diameter".to_string(),
            });
        }
        let area = std::f64::consts::FRAC_PI_4 * effective * effective;
        ValueState::known(Quantity::new(area, UnitRegistry::si().require("m^2")?)?)
    } else {
        ValueState::unavailable(
            "tensile stress area is not established",
            vec!["thread_pitch or revision-bound tensile_area".to_string()],
        )?
    };

    let bolt_stress = match &tensile_area {
        ValueState::Known(area) => {
            let area_m2 = value_in(area, "m^2")?;
            ValueState::known(Quantity::new(
                preload_n / area_m2,
                UnitRegistry::si().require("Pa")?,
            )?)
        }
        ValueState::Unavailable(reason) => ValueState::Unavailable(reason.clone()),
    };

    let utilization = match (&bolt_stress, &input.proof_strength) {
        (ValueState::Known(stress), ValueState::Known(proof)) => {
            require_dimension("proof_strength", proof, Dimension::PRESSURE)?;
            let proof_pa = value_in(proof, "Pa")?;
            require_positive("proof_strength", proof_pa)?;
            let stress_pa = value_in(stress, "Pa")?;
            ValueState::known(stress_pa / proof_pa)
        }
        (ValueState::Unavailable(reason), _) => ValueState::Unavailable(reason.clone()),
        (ValueState::Known(_), ValueState::Unavailable(reason)) => {
            ValueState::Unavailable(reason.clone())
        }
    };

    let mut warnings = Vec::new();
    if matches!(input.nut_factor.uncertainty, Uncertainty::Distribution(_)) {
        warnings.push(warning(
            "uncertainty.normal-envelope",
            "distribution scatter is summarized by its declared numeric envelope; a normal tail is not a hard physical bound",
            WarningSeverity::Info,
        ));
    }
    if !input.proof_strength.is_known() {
        warnings.push(warning(
            "utilization.unavailable",
            "proof strength is unavailable; utilization is not established",
            WarningSeverity::Warning,
        ));
    }
    if !tensile_area.is_known() {
        warnings.push(warning(
            "stress.unavailable",
            "thread pitch or tensile area is missing; bolt stress is not established",
            WarningSeverity::Warning,
        ));
    }

    let mut bcs = BTreeMap::new();
    bcs.insert(
        "torque".to_string(),
        boundary_value(&input.torque, context)?,
    );
    bcs.insert(
        "nominal_diameter".to_string(),
        boundary_value(&input.nominal_diameter, context)?,
    );
    if let Some(pitch) = &input.thread_pitch {
        bcs.insert("thread_pitch".to_string(), boundary_value(pitch, context)?);
    }
    if let Some(area) = &input.tensile_area {
        bcs.insert("tensile_area".to_string(), boundary_value(area, context)?);
    }
    bcs.insert(
        "nut_factor".to_string(),
        boundary_value(
            &Quantity::new(k_nominal, UnitRegistry::si().require("1")?)?,
            context,
        )?,
    );
    if let ValueState::Known(proof) = &input.proof_strength {
        bcs.insert(
            "proof_strength".to_string(),
            boundary_value(proof, context)?,
        );
    }

    let preload_uncertainty = if input.nut_factor.uncertainty.is_exact() {
        Uncertainty::Exact
    } else {
        Uncertainty::Interval(Interval::new(preload_min, preload_max)?)
    };

    let mut results = vec![
        quantity_result("preload_nominal", newton(preload_n)?, preload_uncertainty),
        quantity_result("preload_min", newton(preload_min)?, Uncertainty::Exact),
        quantity_result("preload_max", newton(preload_max)?, Uncertainty::Exact),
    ];
    if let Some(std) = &preload_std_dev {
        results.push(quantity_result(
            "preload_std_dev",
            std.clone(),
            Uncertainty::Exact,
        ));
    }
    match &tensile_area {
        ValueState::Known(area) => {
            results.push(quantity_result(
                "tensile_area",
                area.clone(),
                Uncertainty::Exact,
            ));
        }
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "tensile_area",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    match &bolt_stress {
        ValueState::Known(stress) => {
            results.push(quantity_result(
                "bolt_stress",
                stress.clone(),
                Uncertainty::Exact,
            ));
        }
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "bolt_stress",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }
    match &utilization {
        ValueState::Known(value) => results.push(quantity_result(
            "utilization",
            Quantity::dimensionless(*value)?,
            Uncertainty::Exact,
        )),
        ValueState::Unavailable(reason) => results.push(unavailable_result(
            "utilization",
            reason.reason.clone(),
            reason.required.clone(),
        )?),
    }

    let receipt = build_receipt(
        BOLTED_JOINT_MODEL,
        input,
        MODEL_EXPRESSION,
        &input.nut_factor,
        context,
        bcs,
        Convergence::exact(),
        warnings.clone(),
        results,
        preload_uncertainty,
    )?;

    let torque_back = k_nominal * preload_n * diameter_m;
    let cross_check = Some(CrossCheckEvidence::new(
        "back-substitution of solved preload into T = K F d",
        torque_nm,
        torque_back,
        1e-9 * torque_nm.abs().max(1.0),
    ));

    Ok(ModelReport {
        status: ModelStatus::Solved,
        value: ValueState::known(TorquePreloadResult {
            preload_nominal: newton(preload_n)?,
            preload_bounds: Some(preload_bounds),
            preload_std_dev,
            nut_factor_bounds: k_bounds,
            tensile_area,
            bolt_stress,
            utilization,
        }),
        refusal: None,
        receipt,
        checks,
        assumptions: assumptions(),
        warnings,
        uncertainty: preload_uncertainty,
        cross_check,
    })
}

/// Public record of the model identity used by receipts and tests.
pub fn identity() -> engineering_core::AnalysisIdentity {
    model_identity(BOLTED_JOINT_MODEL)
}

pub const VERSION: &str = MODEL_VERSION;

#[cfg(test)]
mod tests {
    use engineering_core::{Provenance, ProvenanceKind, RevisionId, ValueState};
    use engineering_units::{Distribution, UnitRegistry};

    use super::*;

    fn context() -> ModelContext {
        ModelContext::new(
            RevisionId::new(1),
            Provenance::new(ProvenanceKind::Specification, "test input").unwrap(),
        )
    }

    fn input() -> TorquePreloadInput {
        let units = UnitRegistry::si();
        TorquePreloadInput {
            torque: Quantity::new(100.0, units.require("N*m").unwrap()).unwrap(),
            nominal_diameter: Quantity::new(12.0, units.require("mm").unwrap()).unwrap(),
            thread_pitch: Some(Quantity::new(1.75, units.require("mm").unwrap()).unwrap()),
            tensile_area: None,
            nut_factor: NutFactorScatter::new(
                0.2,
                Uncertainty::Interval(Interval::new(0.18, 0.22).unwrap()),
                Interval::new(0.1, 0.3).unwrap(),
            )
            .unwrap(),
            proof_strength: ValueState::known(
                Quantity::new(640.0, units.require("MPa").unwrap()).unwrap(),
            ),
        }
    }

    #[test]
    fn preload_interval_follows_k_scatter() {
        let report = torque_preload(&input(), &context()).unwrap();
        assert_eq!(report.status, ModelStatus::Solved);
        let result = report.value.as_known().unwrap();
        let min = result.preload_bounds.as_ref().unwrap().min.value();
        let max = result.preload_bounds.as_ref().unwrap().max.value();
        assert!(min < result.preload_nominal.value());
        assert!(max > result.preload_nominal.value());
        assert!(result.utilization.is_known());
    }

    #[test]
    fn missing_proof_strength_leaves_utilization_unavailable() {
        let mut input = input();
        input.proof_strength = ValueState::unavailable(
            "proof strength not supplied",
            vec!["proof strength".to_string()],
        )
        .unwrap();
        let report = torque_preload(&input, &context()).unwrap();
        assert!(!report.value.as_known().unwrap().utilization.is_known());
    }

    #[test]
    fn unrealistic_nut_factor_is_refused() {
        let mut input = input();
        input.nut_factor = NutFactorScatter::new(
            0.2,
            Uncertainty::Distribution(Distribution::uniform(0.01, 0.02).unwrap()),
            Interval::new(0.1, 0.3).unwrap(),
        )
        .unwrap();
        let report = torque_preload(&input, &context()).unwrap();
        assert_eq!(report.status, ModelStatus::Refused);
        assert!(!report.value.is_known());
    }
}
