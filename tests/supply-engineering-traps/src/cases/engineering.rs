use engineering_core::{GateId, ValueState};
use engineering_models::{
    check_moldability, mosfet_conduction_thermal, CheckStatus, Estimate, ModelError, ModelStatus,
    MoldabilityInput, MoldingRules, MosfetThermalInput, RefusalCode,
};
use engineering_units::{Interval, Quantity, Uncertainty, UnitRegistry};

use super::{
    case_error, falsifiable, fixture_provenance, knowledge_pack_ids, model_context, pack_contains,
    pack_evidence, pack_has_id, registry,
};
use crate::{CaseBuilder, CaseOutcome, CaseReport, ExpectedKind, Metrics};

pub(crate) const OPAMP_ID: &str = "analog/opamp-nominal-gain-stability";
pub(crate) const FLYBACK_ID: &str = "power/flyback-leakage-overvoltage";
pub(crate) const PCB_ID: &str = "pcb/alternate-thermal-pad";
pub(crate) const THERMAL_ID: &str = "thermal/missing-contact-resistance";
pub(crate) const MOLDING_ID: &str = "manufacturing/molding-draft-wall";

fn quantity(value: f64, unit: &str) -> Result<Quantity, engineering_units::UnitError> {
    Quantity::new(value, UnitRegistry::si().require(unit)?)
}

fn estimate(value: f64, unit: &str) -> Result<Estimate, crate::BenchError> {
    Estimate::new(
        quantity(value, unit).map_err(|error| case_error("thermal", error))?,
        Uncertainty::Exact,
    )
    .map_err(|error| case_error("thermal", error))
}

fn interval_estimate(
    nominal: f64,
    lower: f64,
    upper: f64,
    unit: &str,
) -> Result<Estimate, crate::BenchError> {
    let interval = Interval::new(lower, upper).map_err(|error| case_error("thermal", error))?;
    Estimate::new(
        quantity(nominal, unit).map_err(|error| case_error("thermal", error))?,
        Uncertainty::Interval(interval),
    )
    .map_err(|error| case_error("thermal", error))
}

fn thermal_input(r_theta_cs: Option<Estimate>) -> Result<MosfetThermalInput, crate::BenchError> {
    Ok(MosfetThermalInput {
        drain_current_rms: interval_estimate(20.0, 19.6, 20.4, "A")?,
        rds_on_reference: interval_estimate(10.0, 9.5, 10.5, "mohm")?,
        temp_coefficient: interval_estimate(0.004, 0.0038, 0.0042, "1/K")?,
        reference_temperature: quantity(25.0, "degC")
            .map_err(|error| case_error("thermal", error))?,
        ambient_temperature: interval_estimate(40.0, 35.0, 45.0, "degC")?,
        r_theta_jc: interval_estimate(1.5, 1.4, 1.6, "K/W")?,
        r_theta_cs,
        r_theta_sa: interval_estimate(2.0, 1.9, 2.1, "K/W")?,
        max_junction_temperature: ValueState::known(
            quantity(150.0, "degC").map_err(|error| case_error("thermal", error))?,
        ),
    })
}

fn contact_resistance() -> Result<Estimate, crate::BenchError> {
    interval_estimate(0.40, 0.35, 0.45, "K/W")
}

pub(crate) fn opamp() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let available = knowledge_pack_ids(&registry);
    let stability_authority = registry.packs().any(|pack| {
        serde_json::to_string(pack).is_ok_and(|text| {
            let text = text.to_lowercase();
            (text.contains("phase margin") || text.contains("loop gain"))
                && (text.contains("op-amp")
                    || text.contains("opamp")
                    || text.contains("operational amplifier"))
        })
    });
    let missing = vec![
        "open-loop gain/phase versus frequency at the actual load and temperature".to_string(),
        "gain-bandwidth product and all open-loop poles/zeros".to_string(),
        "phase margin and gain margin criteria for the closed-loop configuration".to_string(),
        "output load capacitance, ESR and PCB parasitics".to_string(),
        "device and lot variation for the stability-critical parameters".to_string(),
    ];
    let authority = !available.is_empty();
    let reason = if stability_authority {
        "nominal closed-loop gain does not establish loop stability; a bundled pack discusses loop stability, but no executable op-amp stability solver or device loop data is available"
    } else {
        "nominal closed-loop gain does not establish loop stability; no bundled pack supplies an op-amp loop-gain model, and the phase/gain margins are unknown"
    };
    let outcome = CaseOutcome::ResearchRequired {
        gate: GateId::Validity,
        reason: reason.to_string(),
        sources_required: missing.clone(),
    };
    let test = falsifiable(
        "A closed-loop gain of 10 is stable at the specified load and temperature.",
        "Break the loop and measure open-loop gain/phase versus frequency at the worst-case load, output level and temperature; then apply a small input step and record overshoot, ringing and settling.",
        "Phase margin >= 45 deg and gain margin >= 10 dB with no sustained oscillation at any specified corner.",
        "Sustained oscillation, phase margin < 45 deg, gain margin < 10 dB, or load-dependent instability.",
        "Vector network analyzer or loop-gain injection probe plus a high-bandwidth oscilloscope.",
        &[
            "the case supplies only a nominal gain of 10",
            "no device GBW or pole/zero data is present in any bundled knowledge pack",
        ],
    );
    CaseBuilder::new(OPAMP_ID, "analog", "Nominal gain is not a stability proof", ExpectedKind::ResearchRequired)
        .shallow("The closed-loop gain is 10, therefore the amplifier is stable.")
        .outcome(outcome)
        .metrics(
            Metrics::default()
                .with_physics(true)
                .with_authority(authority)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(true),
        )
        .evidence(vec![
            format!("bundled knowledge packs: {}", available.join(", ")),
            if stability_authority {
                "a bundled pack records op-amp loop-stability theory, but no executable stability solver/device data exists".to_string()
            } else {
                "no bundled pack supplies a governing op-amp loop-gain/phase-margin model".to_string()
            },
            "nominal gain is algebra, not a phase-margin calculation".to_string(),
        ])
        .missing(missing)
        .boundaries(vec![
            "stability boundary: phase margin > 0 deg; the design acceptance boundary here is >= 45 deg and gain margin >= 10 dB".to_string(),
            "the boundary cannot be evaluated without open-loop gain/phase data".to_string(),
        ])
        .uncertainty("No uncertainty is claimed for a quantity that was never established; the stability margin remains unknown.")
        .test(test)
        .build()
}

pub(crate) fn flyback() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let pack = registry
        .latest("power-electronics")
        .ok_or_else(|| case_error(FLYBACK_ID, "power-electronics pack is missing"))?;
    let assumption = pack
        .assumptions
        .iter()
        .find(|item| item.id == "asm-conduction-only")
        .ok_or_else(|| case_error(FLYBACK_ID, "conduction-only assumption is missing"))?;
    let excluded = ["switching", "avalanche", "leakage"]
        .iter()
        .all(|needle| assumption.statement.to_lowercase().contains(needle));
    let authority = excluded && pack.authority().is_some();
    let missing = vec![
        "transformer leakage inductance and coupling coefficient".to_string(),
        "primary current slope at turn-off and the actual switching waveform".to_string(),
        "snubber/clamp topology and its parasitic elements".to_string(),
        "MOSFET Vds rating, avalanche energy rating and thermal impedance at the switching transient".to_string(),
        "worst-case high-line, full-load and maximum-temperature operating point".to_string(),
    ];
    let outcome = CaseOutcome::ResearchRequired {
        gate: GateId::Theory,
        reason: format!(
            "the bundled power-electronics authority is conduction-only and explicitly excludes the relevant physics: {:?}",
            assumption.statement
        ),
        sources_required: missing.clone(),
    };
    let test = falsifiable(
        "The reflected voltage Vin + N*Vout is the complete MOSFET drain-source stress.",
        "Measure Vds with a high-voltage differential probe and primary current with a current shunt at worst-case high line, full load and maximum temperature, including turn-off.",
        "Peak Vds stays below the datasheet Vds rating with margin, and avalanche energy stays below the rated avalanche energy.",
        "Peak Vds reaches the rating, enters avalanche, or the measured leakage spike invalidates the reflected-voltage-only estimate.",
        "High-voltage differential probe, current shunt, isolated high-bandwidth oscilloscope.",
        &[
            "the pack assumption says switching, avalanche and leakage losses are separate",
            "no leakage-inductance or snubber model is present",
        ],
    );
    let mut evidence = pack_evidence(&registry, "power-electronics")?;
    evidence.push(format!(
        "assumption asm-conduction-only: {:?}",
        assumption.statement
    ));
    evidence.push("the model is conduction-only; a reflected-voltage-only stress claim is outside its authority".to_string());
    CaseBuilder::new(FLYBACK_ID, "power-electronics", "Flyback leakage spike is outside the conduction-only model", ExpectedKind::ResearchRequired)
        .shallow("Vds_peak = Vin + N*Vout is below the MOSFET rating, so the switch is safe.")
        .outcome(outcome)
        .metrics(
            Metrics::default()
                .with_physics(true)
                .with_authority(authority)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(true)
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(true),
        )
        .evidence(evidence)
        .missing(missing)
        .boundaries(vec![
            "peak Vds must remain below the device Vds rating at the worst-case operating corner".to_string(),
            "avalanche energy must remain below the rated avalanche energy if the clamp is not ideal".to_string(),
            "the reflected voltage alone does not bound the leakage-inductance spike".to_string(),
        ])
        .uncertainty("The leakage spike is not bounded by any supplied uncertainty because the leakage/switch model is absent; the result is research-required, not a nominal number.")
        .test(test)
        .build()
}

pub(crate) fn pcb_alternate() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let original_input = thermal_input(Some(contact_resistance()?))?;
    let original = mosfet_conduction_thermal(&original_input, &model_context())
        .map_err(|error| case_error(PCB_ID, error))?;
    let alternate_input = thermal_input(None)?;
    let alternate = mosfet_conduction_thermal(&alternate_input, &model_context())
        .map_err(|error| case_error(PCB_ID, error))?;

    let original_value = original.value.as_known();
    let alternate_refused = alternate.status == ModelStatus::Refused
        && alternate
            .refusal
            .as_ref()
            .is_some_and(|refusal| refusal.code == RefusalCode::MissingInterfaceResistance);
    let physics =
        original.status == ModelStatus::Solved && original_value.is_some() && alternate_refused;
    let authority = pack_has_id(&registry, "power-electronics", "prop-contact-rth")?
        && pack_has_id(&registry, "power-electronics", "rule-contact-resistance")?;
    let mut evidence = pack_evidence(&registry, "power-electronics")?;
    evidence
        .push("property prop-contact-rth and rule rule-contact-resistance are present".to_string());
    if let Some(value) = original_value {
        evidence.push(format!(
            "original assembly: Tj = {:.3} degC with Rth_cs = 0.35..0.45 K/W",
            value.junction_temperature.value()
        ));
    }
    if let Some(refusal) = &alternate.refusal {
        evidence.push(format!(
            "alternate assembly refusal: {:?}: {}",
            refusal.code, refusal.reason
        ));
    }
    let test = falsifiable(
        "The alternate PCB is thermally equivalent because it has the same copper area and MOSFET.",
        "Measure case-to-sink thermal resistance and junction temperature on the actual alternate assembly at the specified mounting pressure, power and airflow; repeat at worst-case ambient.",
        "The alternate assembly meets the junction-temperature margin with a measured interface resistance no worse than the assumed value.",
        "The alternate pad raises interface resistance or junction temperature beyond the margin, or the interface cannot be characterized.",
        "JEDEC JESD51-14 thermal tester, thermocouples/TSP, torque-controlled mounting fixture.",
        &[
            "the alternate pad geometry and interface material are different",
            "no interface resistance evidence exists for the alternate",
        ],
    );
    let mut missing = vec![
        "alternate PCB thermal-pad geometry, copper spreading and via array".to_string(),
        "thermal interface material, thickness and mounting pressure for the alternate".to_string(),
        "measured case-to-sink resistance on the actual alternate assembly".to_string(),
        "airflow and enclosure boundary conditions for the alternate".to_string(),
    ];
    if let Some(refusal) = &alternate.refusal {
        missing.extend(refusal.evidence.clone());
    }
    CaseBuilder::new(PCB_ID, "pcb/thermal", "Alternate PCB with a different thermal pad is not equivalent", ExpectedKind::Refused)
        .shallow("Same copper area and same MOSFET means the alternate PCB is thermally equivalent.")
        .outcome(CaseOutcome::Refused {
            code: format!("{:?}", RefusalCode::MissingInterfaceResistance),
            reason: alternate
                .refusal
                .as_ref()
                .map(|refusal| refusal.reason.clone())
                .unwrap_or_else(|| "alternate interface resistance is unavailable".to_string()),
            required: missing.clone(),
            receipt_bound: alternate.refusal.is_some(),
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(authority)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(original_value.is_some_and(|value| value.junction_interval_kelvin.is_some()))
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(alternate_refused),
        )
        .evidence(evidence)
        .missing(missing)
        .boundaries(vec![
            "case-to-sink interface resistance is a required thermal boundary condition".to_string(),
            "junction temperature must stay below the maximum rated junction temperature".to_string(),
        ])
        .uncertainty("The original assembly carries an explicit interval; the alternate's interface resistance is unknown, so no alternate junction temperature is asserted.")
        .test(test)
        .build()
}

pub(crate) fn thermal_contact() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let with_contact = thermal_input(Some(contact_resistance()?))?;
    let without_contact = thermal_input(None)?;
    let solved = mosfet_conduction_thermal(&with_contact, &model_context())
        .map_err(|error| case_error(THERMAL_ID, error))?;
    let refused = mosfet_conduction_thermal(&without_contact, &model_context())
        .map_err(|error| case_error(THERMAL_ID, error))?;
    let zero = thermal_input(Some(estimate(0.0, "K/W")?))?;
    let zero_result = mosfet_conduction_thermal(&zero, &model_context());
    let zero_rejected = matches!(
        zero_result,
        Err(ModelError::InvalidInput {
            field: "r_theta_cs",
            ..
        })
    );
    let solved_value = solved.value.as_known();
    let refused_missing = refused.status == ModelStatus::Refused
        && refused
            .refusal
            .as_ref()
            .is_some_and(|refusal| refusal.code == RefusalCode::MissingInterfaceResistance);
    let physics = solved.status == ModelStatus::Solved
        && solved_value.is_some()
        && refused_missing
        && zero_rejected;
    let authority = pack_has_id(&registry, "power-electronics", "prop-contact-rth")?
        && pack_has_id(&registry, "power-electronics", "rule-contact-resistance")?
        && pack_contains(
            &registry,
            "power-electronics",
            "trap-missing-contact-resistance",
        )?;
    let mut evidence = pack_evidence(&registry, "power-electronics")?;
    evidence
        .push("property prop-contact-rth and rule rule-contact-resistance are present".to_string());
    evidence.push("trap-missing-contact-resistance is recorded in the pack".to_string());
    if let Some(value) = solved_value {
        evidence.push(format!(
            "with Rth_cs: Tj = {:.3} degC, loss = {:.3} W, margin = {:?}",
            value.junction_temperature.value(),
            value.conduction_loss.value(),
            value.thermal_margin
        ));
    }
    if let Some(refusal) = &refused.refusal {
        evidence.push(format!(
            "without Rth_cs refusal: {:?}: {}",
            refusal.code, refusal.reason
        ));
    }
    evidence.push(format!("r_theta_cs = 0 is rejected: {zero_rejected}"));
    let test = falsifiable(
        "The junction-to-case and sink-to-ambient resistances alone determine the junction temperature.",
        "Measure the case-to-sink interface resistance and junction temperature on the actual assembly at the production mounting pressure and worst-case ambient; compare with the assumed thermal chain.",
        "The measured interface resistance is no greater than the value used in the calculation and the junction-temperature margin remains positive.",
        "The interface resistance or junction temperature exceeds the calculated value, or the interface is uncharacterized.",
        "JEDEC JESD51-14 tester, thermal test die or TSP, calibrated thermocouples, torque-controlled fixture.",
        &[
            "the missing-interface case is refused rather than assigned Rth_cs = 0",
            "a supplied zero interface resistance is rejected as non-positive",
        ],
    );
    CaseBuilder::new(THERMAL_ID, "thermal", "Ignoring contact resistance understates junction temperature", ExpectedKind::Refused)
        .shallow("Use Rth_jc + Rth_sa; the thermal interface is negligible.")
        .outcome(CaseOutcome::Refused {
            code: format!("{:?}", RefusalCode::MissingInterfaceResistance),
            reason: refused
                .refusal
                .as_ref()
                .map(|refusal| refusal.reason.clone())
                .unwrap_or_else(|| "interface resistance is unavailable".to_string()),
            required: refused
                .refusal
                .as_ref()
                .map(|refusal| refusal.evidence.clone())
                .unwrap_or_default(),
            receipt_bound: refused.refusal.is_some(),
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(authority)
                .with_missing_info(true)
                .with_boundaries(true)
                .with_uncertainty(solved_value.is_some_and(|value| value.junction_interval_kelvin.is_some()))
                .with_trap(true)
                .with_experiment(true)
                .with_refusal(refused_missing && zero_rejected),
        )
        .evidence(evidence)
        .missing(vec![
            "revision-bound thermal interface material data at the assembly pressure".to_string(),
            "measured case-to-sink resistance for the actual stack".to_string(),
        ])
        .boundaries(vec![
            "Rth_cs must be strictly positive; a missing or zero interface is not a valid boundary condition".to_string(),
            "junction temperature must stay below the maximum rated junction temperature".to_string(),
        ])
        .uncertainty("The solved control propagates an explicit interval; the missing-interface case is refused and reports no fabricated exact temperature.")
        .test(test)
        .build()
}

fn check_status(result: &engineering_models::MoldabilityResult, id: &str) -> Option<CheckStatus> {
    result
        .checks
        .iter()
        .find(|check| check.id == id)
        .map(|check| check.status)
}

fn molding_input(
    nominal: f64,
    min: f64,
    max: f64,
    draft: f64,
    draft_unit: &str,
    flow: f64,
) -> Result<MoldabilityInput, crate::BenchError> {
    let units = UnitRegistry::si();
    let rules = MoldingRules::new(
        Quantity::new(
            1.0,
            units
                .require("deg")
                .map_err(|error| case_error(MOLDING_ID, error))?,
        )
        .map_err(|error| case_error(MOLDING_ID, error))?,
        0.12,
        Some(150.0),
        fixture_provenance(
            "benchmark fixture: explicit DFM rule inputs, not a universal draft/wall standard",
        ),
    )
    .map_err(|error| case_error(MOLDING_ID, error))?;
    Ok(MoldabilityInput {
        nominal_wall: Quantity::new(
            nominal,
            units
                .require("mm")
                .map_err(|error| case_error(MOLDING_ID, error))?,
        )
        .map_err(|error| case_error(MOLDING_ID, error))?,
        min_wall: Quantity::new(
            min,
            units
                .require("mm")
                .map_err(|error| case_error(MOLDING_ID, error))?,
        )
        .map_err(|error| case_error(MOLDING_ID, error))?,
        max_wall: Quantity::new(
            max,
            units
                .require("mm")
                .map_err(|error| case_error(MOLDING_ID, error))?,
        )
        .map_err(|error| case_error(MOLDING_ID, error))?,
        draft_actual: Quantity::new(
            draft,
            units
                .require(draft_unit)
                .map_err(|error| case_error(MOLDING_ID, error))?,
        )
        .map_err(|error| case_error(MOLDING_ID, error))?,
        flow_length: Some(
            Quantity::new(
                flow,
                units
                    .require("mm")
                    .map_err(|error| case_error(MOLDING_ID, error))?,
            )
            .map_err(|error| case_error(MOLDING_ID, error))?,
        ),
        rules,
    })
}

pub(crate) fn molding() -> Result<CaseReport, crate::BenchError> {
    let registry = registry()?;
    let bad = molding_input(2.5, 2.2, 2.9, 0.5, "deg", 400.0)?;
    let bad_report =
        check_moldability(&bad, &model_context()).map_err(|error| case_error(MOLDING_ID, error))?;
    let radians = molding_input(2.5, 2.2, 2.9, 0.5, "rad", 400.0)?;
    let radians_report = check_moldability(&radians, &model_context())
        .map_err(|error| case_error(MOLDING_ID, error))?;
    let good = molding_input(2.5, 2.4, 2.6, 1.5, "deg", 300.0)?;
    let good_report = check_moldability(&good, &model_context())
        .map_err(|error| case_error(MOLDING_ID, error))?;

    let bad_draft = check_status(
        bad_report
            .value
            .as_known()
            .ok_or_else(|| case_error(MOLDING_ID, "no bad result"))?,
        "draft",
    );
    let radian_draft = check_status(
        radians_report
            .value
            .as_known()
            .ok_or_else(|| case_error(MOLDING_ID, "no radian result"))?,
        "draft",
    );
    let bad_value = bad_report
        .value
        .as_known()
        .ok_or_else(|| case_error(MOLDING_ID, "no bad value"))?;
    let good_value = good_report
        .value
        .as_known()
        .ok_or_else(|| case_error(MOLDING_ID, "no good value"))?;
    let physics = !bad_value.passes_all_required
        && bad_draft == Some(CheckStatus::Fail)
        && radian_draft == Some(CheckStatus::Pass)
        && good_value.passes_all_required;
    let authority = [
        "rule-draft",
        "rule-wall-uniformity",
        "rule-flow-ratio",
        "trap-draft-units",
    ]
    .iter()
    .map(|id| pack_has_id(&registry, "injection-molding", id))
    .collect::<Result<Vec<_>, _>>()?
    .into_iter()
    .all(|value| value);
    let mut evidence = pack_evidence(&registry, "injection-molding")?;
    evidence
        .push("rules rule-draft, rule-wall-uniformity and rule-flow-ratio are present".to_string());
    evidence.push("trap-draft-units is recorded in the pack".to_string());
    evidence.push(format!(
        "bad design checks: draft={:?}, wall={:?}, flow={:?}",
        bad_draft,
        check_status(bad_value, "wall_uniformity"),
        check_status(bad_value, "flow_length_ratio")
    ));
    evidence.push(format!(
        "good control passes all required checks: {}",
        good_value.passes_all_required
    ));
    let test = falsifiable(
        "The as-drawn wall and draft are manufacturable in the production tool.",
        "Run a first-article mold trial and inspect the as-molded wall thickness, draft angle and flow length at the specified material and texture; compare against the explicit DFM rules.",
        "All required draft, wall-uniformity and flow-ratio checks pass on the first article, with no short shot, drag or warpage.",
        "A required check fails on the first article, or the measured geometry differs from CAD enough to violate a limit.",
        "CMM or structured-light scan, draft gauge, mold-trial inspection report.",
        &[
            "the bad case has 0.5 deg draft against a 1.0 deg requirement",
            "the same 0.5 value in radians is 28.6 deg and compares differently",
            "wall variation is 0.28 against a 0.12 limit and flow ratio is 160 against 150",
        ],
    );
    CaseBuilder::new(MOLDING_ID, "manufacturing", "Draft and wall violations make the molded geometry unmanufacturable", ExpectedKind::Rejected)
        .shallow("The CAD model closes, so the molded part can be produced.")
        .outcome(CaseOutcome::Rejected {
            code: "MoldabilityViolation".to_string(),
            reasons: bad_value
                .checks
                .iter()
                .filter(|check| check.status == CheckStatus::Fail)
                .map(|check| format!("{}: {}", check.id, check.message))
                .collect(),
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
                .with_refusal(true),
        )
        .evidence(evidence)
        .missing(vec![
            "material/tool-specific minimum draft and wall limits".to_string(),
            "texture depth and ejection design".to_string(),
        ])
        .boundaries(vec![
            "draft_actual >= draft_min_required".to_string(),
            "wall_variation <= max_wall_variation".to_string(),
            "flow_ratio <= max_flow_ratio".to_string(),
        ])
        .uncertainty("The checks are deterministic for the supplied geometry and explicit rule inputs; they do not claim a universal DFM threshold.")
        .test(test)
        .build()
}
