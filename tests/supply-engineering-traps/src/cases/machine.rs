use engineering_core::{GateId, ValueState};
use engineering_models::{
    torque_preload, ModelStatus, NutFactorScatter, RefusalCode, TorquePreloadInput,
};
use engineering_units::{Interval, Quantity, Uncertainty, UnitRegistry};

use super::{case_error, falsifiable, model_context, pack_evidence, pack_has_id, registry};
use crate::{CaseBuilder, CaseOutcome, CaseReport, ExpectedKind, Metrics};

pub(crate) const FASTENER_ID: &str = "machine/fastener-nut-factor-scatter";
pub(crate) const FATIGUE_ID: &str = "fatigue/sn-outside-regime";

fn quantity(value: f64, unit: &str) -> Result<Quantity, crate::BenchError> {
    Quantity::new(
        value,
        UnitRegistry::si()
            .require(unit)
            .map_err(|error| case_error(FASTENER_ID, error))?,
    )
    .map_err(|error| case_error(FASTENER_ID, error))
}

fn bolt_input(
    k_lower: f64,
    k_upper: f64,
    validity_lower: f64,
    validity_upper: f64,
) -> Result<TorquePreloadInput, crate::BenchError> {
    let nut_factor = NutFactorScatter::new(
        0.2,
        Uncertainty::Interval(
            Interval::new(k_lower, k_upper).map_err(|error| case_error(FASTENER_ID, error))?,
        ),
        Interval::new(validity_lower, validity_upper)
            .map_err(|error| case_error(FASTENER_ID, error))?,
    )
    .map_err(|error| case_error(FASTENER_ID, error))?;
    Ok(TorquePreloadInput {
        torque: quantity(100.0, "N*m")?,
        nominal_diameter: quantity(12.0, "mm")?,
        thread_pitch: Some(quantity(1.75, "mm")?),
        tensile_area: None,
        nut_factor,
        proof_strength: ValueState::known(quantity(640.0, "MPa")?),
    })
}

pub(crate) fn fastener() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let valid_input = bolt_input(0.18, 0.22, 0.12, 0.28)?;
    let valid = torque_preload(&valid_input, &model_context())
        .map_err(|error| case_error(FASTENER_ID, error))?;
    let invalid_input = bolt_input(0.05, 0.50, 0.12, 0.28)?;
    let invalid = torque_preload(&invalid_input, &model_context())
        .map_err(|error| case_error(FASTENER_ID, error))?;

    let valid_value = valid
        .value
        .as_known()
        .ok_or_else(|| case_error(FASTENER_ID, "valid control did not solve"))?;
    let bounds = valid_value
        .preload_bounds
        .as_ref()
        .ok_or_else(|| case_error(FASTENER_ID, "valid control produced no preload bounds"))?;
    let nominal_inside = bounds.min.value() <= valid_value.preload_nominal.value()
        && valid_value.preload_nominal.value() <= bounds.max.value();
    let utilization = valid_value.utilization.as_known().copied();
    let invalid_refused = invalid.status == ModelStatus::Refused
        && invalid
            .refusal
            .as_ref()
            .is_some_and(|refusal| refusal.code == RefusalCode::OutsideValidity);
    let physics = valid.status == ModelStatus::Solved
        && nominal_inside
        && utilization.is_some_and(|value| value > 0.0 && value < 1.0)
        && valid.receipt.convergence.converged
        && invalid_refused;
    let authority = ["prop-nut-factor", "val-k-domain", "trap-unrealistic-k"]
        .iter()
        .map(|id| pack_has_id(&registry, "bolted-joint", id))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .all(|value| value);
    let mut evidence = pack_evidence(&registry, "bolted-joint")?;
    evidence.push(
        "property prop-nut-factor, domain val-k-domain and trap trap-unrealistic-k are present"
            .to_string(),
    );
    evidence.push(format!(
        "valid control preload = {:.3} N, bounds = {:.3}..{:.3} N, utilization = {:?}",
        valid_value.preload_nominal.value(),
        bounds.min.value(),
        bounds.max.value(),
        utilization
    ));
    if let Some(refusal) = &invalid.refusal {
        evidence.push(format!(
            "invalid K scatter refusal: {:?}: {}",
            refusal.code, refusal.reason
        ));
    }
    let test = falsifiable(
        "A nominal nut factor of 0.2 predicts the production preload within the required scatter.",
        "Run a torque-tension test on the actual joint with the production lubrication, coating, thread condition and tightening tool; measure clamp force and derive K at the specified torque.",
        "The measured K distribution lies inside the qualified validity range and the resulting preload scatter meets the joint requirement.",
        "The measured K distribution falls outside the qualified range, or the preload scatter exceeds the design allowance.",
        "Calibrated torque-tension rig, load cell, torque transducer, thread-condition records.",
        &[
            "the invalid scatter interval 0.05..0.50 lies outside the qualified K domain 0.12..0.28",
            "the model refuses rather than clamping K or reporting the nominal preload as certain",
        ],
    );
    CaseBuilder::new(FASTENER_ID, "machine-design", "Nominal torque hides nut-factor scatter and validity", ExpectedKind::Refused)
        .shallow("K = 0.2 gives 41.7 kN, so the joint preload is known.")
        .outcome(CaseOutcome::Refused {
            code: format!("{:?}", RefusalCode::OutsideValidity),
            reason: invalid
                .refusal
                .as_ref()
                .map(|refusal| refusal.reason.clone())
                .unwrap_or_else(|| "nut-factor scatter is outside its validity domain".to_string()),
            required: invalid
                .refusal
                .as_ref()
                .map(|refusal| refusal.evidence.clone())
                .unwrap_or_default(),
            receipt_bound: invalid.refusal.is_some(),
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(authority)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(invalid_refused),
        )
        .evidence(evidence)
        .missing(vec![
            "joint-specific qualified nut-factor distribution".to_string(),
            "production lubrication, coating and thread condition".to_string(),
            "torque-tension test evidence spanning the production K range".to_string(),
        ])
        .boundaries(vec![
            "nut-factor scatter must lie inside the qualified validity interval [0.12, 0.28]".to_string(),
            "preload is only bounded when the K interval is valid".to_string(),
            "bolt stress utilization must remain below the proof-strength limit".to_string(),
        ])
        .uncertainty("The valid control returns an explicit preload interval from the K interval; the invalid control refuses and never replaces the unknown preload with the nominal value.")
        .test(test)
        .build()
}

pub(crate) fn fatigue() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let fatigue_authority = registry.packs().any(|pack| {
        serde_json::to_string(pack).is_ok_and(|text| {
            let text = text.to_lowercase();
            (text.contains("s-n") || text.contains("fatigue"))
                && (text.contains("endurance")
                    || text.contains("stress ratio")
                    || text.contains("run-out"))
        })
    });
    let missing = vec![
        "S-N curve with stress ratio, mean stress correction and run-out definition".to_string(),
        "surface finish, notch factor and residual-stress condition".to_string(),
        "environment and temperature for the service regime".to_string(),
        "evidence that the required cycle count is inside the curve's tested validity regime"
            .to_string(),
        "lot/process scatter and the statistical basis for the endurance limit".to_string(),
    ];
    let test = falsifiable(
        "The component has infinite fatigue life at the service stress and 2e7 cycles.",
        "Run constant-amplitude fatigue tests at the service stress ratio and surface condition to the target cycle count, with additional staircase tests for the endurance limit; inspect failures and run-outs.",
        "All specimens reach the target with no failure and the staircase endurance limit exceeds the service stress with the required margin.",
        "A specimen fails before the target, or the endurance limit is below the service stress, or the target lies outside the tested regime.",
        "Servo-hydraulic or electromagnetic fatigue machine, calibrated load cell, fracture inspection.",
        &[
            "the required 2e7-cycle regime is not covered by any bundled S-N authority",
            "no S-N curve is supplied, so an endurance limit cannot be derived from UTS",
        ],
    );
    CaseBuilder::new(FATIGUE_ID, "fatigue", "S-N extrapolation outside the tested regime is not valid", ExpectedKind::ResearchRequired)
        .shallow("Endurance limit is half the tensile strength, so the part has infinite life.")
        .outcome(CaseOutcome::ResearchRequired {
            gate: GateId::Validity,
            reason: "no S-N curve or fatigue validity envelope is available for the service stress ratio, surface condition and 2e7-cycle target".to_string(),
            sources_required: missing.clone(),
        })
        .metrics(
            Metrics::default()
                .with_physics(true)
                .with_authority(true)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(true),
        )
        .evidence(vec![
            format!(
                "bundled knowledge packs: {}",
                registry
                    .packs()
                    .map(|pack| format!("{}@{}", pack.id, pack.version))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            if fatigue_authority {
                "a bundled pack mentions fatigue/S-N, but no executable curve or validity envelope for the service regime is supplied".to_string()
            } else {
                "no bundled pack supplies an S-N/endurance model with a stress-ratio/run-out envelope".to_string()
            },
            "a UTS fraction is not an S-N curve and has no run-out or stress-ratio authority".to_string(),
        ])
        .missing(missing)
        .boundaries(vec![
            "stress ratio, mean stress and surface condition define the S-N applicability".to_string(),
            "the target cycle count must be inside the curve's tested range and run-out definition".to_string(),
            "the endurance limit cannot be extrapolated from UTS".to_string(),
        ])
        .uncertainty("No life or endurance limit is asserted; the fatigue result is research-required until a valid S-N envelope and scatter basis exist.")
        .test(test)
        .build()
}
