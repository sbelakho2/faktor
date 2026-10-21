use engineering_core::{
    Applicability, EngineeringValue, MaterialRef, Provenance, ProvenanceKind, RevisionId,
    ThicknessRange, ValueState,
};
use engineering_models::{
    emc_pcb_check, fatigue_damage, flyback_converter, lookup_material_property,
    opamp_loop_stability, thermal_network, EmcPcbInput, EmcRules, FatigueBlock, FatigueInput,
    FlybackInput, FlybackMode, MaterialPropertyQuery, MaterialPropertyRecord, MeanStressCorrection,
    ModelContext, ModelStatus, OpAmpStabilityInput, PcbStackup, RefusalCode, SnCurve,
    StrainLifeCurve, ThermalLayer, ThermalNetworkInput,
};
use engineering_units::{Interval, Quantity, UnitRegistry};

fn context() -> ModelContext {
    ModelContext::new(
        RevisionId::new(7),
        Provenance::new(ProvenanceKind::Specification, "domain model test fixture").unwrap(),
    )
}

fn q(value: f64, unit: &str) -> Quantity {
    Quantity::new(value, UnitRegistry::si().require(unit).unwrap()).unwrap()
}

fn analog_input() -> OpAmpStabilityInput {
    OpAmpStabilityInput {
        open_loop_gain: 100_000.0,
        first_pole: q(10.0, "Hz"),
        second_pole: q(1_000_000.0, "Hz"),
        output_resistance: Some(q(100.0, "ohm")),
        load_capacitance: Some(q(1.0, "uF")),
        opamp_output_capacitance: Some(q(10.0, "pF")),
        input_capacitance: Some(q(1.0, "nF")),
        feedback_resistance: Some(q(10_000.0, "ohm")),
        feedback_capacitor: None,
        noise_gain: 10.0,
        required_phase_margin_deg: 45.0,
        required_gain_margin_db: 10.0,
    }
}

#[test]
fn every_domain_receipt_carries_checks_results_and_assumptions() {
    let analog = opamp_loop_stability(&analog_input(), &context()).unwrap();
    let flyback = flyback_converter(&flyback_input(), &context()).unwrap();
    let thermal = thermal_network(&thermal_input(), &context()).unwrap();
    let fatigue = fatigue_damage(&fatigue_input(), &context()).unwrap();
    let emc = emc_pcb_check(&emc_input(), &context()).unwrap();
    let materials = lookup_material_property(
        &material_query("6061", "T651", 80.0),
        &[
            property_record("thin-t6", "aluminium", "6061", "T6", (0.0, 12.0), 276.0),
            property_record(
                "plate-t651",
                "aluminium",
                "6061",
                "T651",
                (50.0, 100.0),
                250.0,
            ),
        ],
        &context(),
    )
    .unwrap();
    let receipts = [
        &analog.receipt,
        &flyback.receipt,
        &thermal.receipt,
        &fatigue.receipt,
        &emc.receipt,
        &materials.receipt,
    ];
    let checks = [
        &analog.checks,
        &flyback.checks,
        &thermal.checks,
        &fatigue.checks,
        &emc.checks,
        &materials.checks,
    ];
    let assumptions = [
        &analog.assumptions,
        &flyback.assumptions,
        &thermal.assumptions,
        &fatigue.assumptions,
        &emc.assumptions,
        &materials.assumptions,
    ];
    for index in 0..receipts.len() {
        let receipt = receipts[index];
        assert!(receipt.executed, "{}", receipt.identity.solver_id);
        assert!(!receipt.results.is_empty());
        assert!(!checks[index].is_empty());
        assert!(!assumptions[index].is_empty());
        receipt.validate().unwrap();
    }
}

#[test]
fn opamp_load_capacitance_can_destabilize_a_nominal_gain_of_ten() {
    let report = opamp_loop_stability(&analog_input(), &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Solved);
    let value = report.value.as_known().unwrap();
    assert!(
        !value.stable,
        "the capacitive load must destroy the phase margin"
    );
    assert!(value.phase_margin.as_known().unwrap().value() < 0.0);
    assert!(value.compensation_required);
    let required_gain = value.required_noise_gain.unwrap();
    assert!(required_gain > input_noise_gain());
    assert!(report.receipt.executed);
    assert!(
        report.cross_check.as_ref().unwrap().passed,
        "{:?}",
        report.cross_check
    );
}

#[test]
fn required_noise_gain_restores_the_margin() {
    let report = opamp_loop_stability(&analog_input(), &context()).unwrap();
    let required = report
        .value
        .as_known()
        .unwrap()
        .required_noise_gain
        .unwrap();
    let mut gain_input = analog_input();
    gain_input.noise_gain = required;
    let gain_report = opamp_loop_stability(&gain_input, &context()).unwrap();
    assert!(
        gain_report
            .value
            .as_known()
            .unwrap()
            .phase_margin
            .as_known()
            .unwrap()
            .value()
            >= 45.0
    );
}

fn input_noise_gain() -> f64 {
    analog_input().noise_gain
}

#[test]
fn opamp_missing_load_is_typed_unavailable() {
    let mut input = analog_input();
    input.load_capacitance = None;
    let report = opamp_loop_stability(&input, &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Unavailable);
    assert!(!report.value.is_known());
}

#[test]
fn opamp_unordered_poles_are_outside_validity() {
    let mut input = analog_input();
    input.second_pole = q(5.0, "Hz");
    let report = opamp_loop_stability(&input, &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Refused);
    assert_eq!(
        report.refusal.as_ref().unwrap().code,
        RefusalCode::OutsideValidity
    );
}

fn flyback_input() -> FlybackInput {
    FlybackInput {
        input_voltage: q(24.0, "V"),
        output_voltage: q(12.0, "V"),
        output_current: q(2.0, "A"),
        switching_frequency: q(100_000.0, "Hz"),
        primary_inductance: q(100.0, "uH"),
        turns_ratio: 2.0,
        leakage_inductance: Some(q(2.0, "uH")),
        clamp_voltage: Some(q(60.0, "V")),
        clamp_ripple_fraction: 0.1,
        output_capacitance: Some(q(220.0, "uF")),
        output_esr: Some(q(0.05, "ohm")),
        vds_rating: Some(q(100.0, "V")),
        voltage_derating: 0.8,
    }
}

#[test]
fn flyback_ccm_currents_and_leakage_clamp_match_closed_form() {
    let report = flyback_converter(&flyback_input(), &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert_eq!(value.mode, FlybackMode::Ccm);
    assert!((value.duty - 0.5).abs() < 1e-12);
    assert!((value.primary_peak_current.value() - 2.6).abs() < 1e-9);
    assert!((value.primary_valley_current.value() - 1.4).abs() < 1e-9);
    assert!((value.secondary_peak_current.value() - 5.2).abs() < 1e-9);
    assert!(
        (value.snubber_loss.as_known().unwrap().value() - 1.126_666_666_666_666_7).abs() < 1e-9,
        "{:?}",
        value.snubber_loss
    );
    assert!(
        (value.clamp_resistance.as_known().unwrap().value() - 3_195.266_272_189_349).abs() < 1e-6
    );
    assert!((value.vds_peak.as_known().unwrap().value() - 84.0).abs() < 1e-12);
    assert_eq!(value.overstress, Some(true));
    assert!((value.output_ripple.as_known().unwrap().value() - 0.3054545454545455).abs() < 1e-9);
    assert!(report.cross_check.as_ref().unwrap().passed);
}

#[test]
fn flyback_dcm_mode_is_detected_from_the_boundary_condition() {
    let mut input = flyback_input();
    input.primary_inductance = q(10.0, "uH");
    let report = flyback_converter(&input, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert_eq!(value.mode, FlybackMode::Dcm);
    assert!((value.primary_valley_current.value() - 0.0).abs() < 1e-12);
    assert!(value.primary_peak_current.value() > 0.0);
}

#[test]
fn flyback_missing_leakage_keeps_stress_unavailable() {
    let mut input = flyback_input();
    input.leakage_inductance = None;
    let report = flyback_converter(&input, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert!(!value.leakage_energy_per_cycle.is_known());
    assert!(!value.snubber_loss.is_known());
    assert!(!value.vds_peak.is_known());
    assert_eq!(value.overstress, None);
}

#[test]
fn flyback_clamp_below_reflected_voltage_is_refused() {
    let mut input = flyback_input();
    input.clamp_voltage = Some(q(20.0, "V"));
    let report = flyback_converter(&input, &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Refused);
    assert_eq!(
        report.refusal.as_ref().unwrap().code,
        RefusalCode::OutsideValidity
    );
}

fn thermal_input() -> ThermalNetworkInput {
    ThermalNetworkInput {
        power: q(50.0, "W"),
        ambient: q(25.0, "degC"),
        junction_to_case: ThermalLayer {
            resistance: q(0.5, "K/W"),
            capacitance: Some(q(1.0, "J/K")),
        },
        case_to_sink: Some(ThermalLayer {
            resistance: q(0.2, "K/W"),
            capacitance: Some(q(2.0, "J/K")),
        }),
        sink_to_ambient: ThermalLayer {
            resistance: q(1.0, "K/W"),
            capacitance: Some(q(10.0, "J/K")),
        },
        transient_times: vec![q(0.1, "s"), q(1.0, "s"), q(10.0, "s"), q(100.0, "s")],
        max_junction_temperature: ValueState::known(q(150.0, "degC")),
    }
}

#[test]
fn thermal_network_steady_state_and_transient_ladder_match_closed_form() {
    let report = thermal_network(&thermal_input(), &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert!((value.steady_junction.value() - 110.0).abs() < 1e-9);
    assert!((value.steady_case.value() - 85.0).abs() < 1e-9);
    assert!((value.steady_sink.value() - 75.0).abs() < 1e-9);
    assert!((value.thermal_resistance_total.value() - 1.7).abs() < 1e-12);
    let points = value.transient.as_known().unwrap();
    assert_eq!(points.len(), 4);
    assert!(points[0].junction.value() < points[1].junction.value());
    assert!(points[2].junction.value() < points[3].junction.value());
    assert!((points[3].junction.value() - 110.0).abs() < 1.0);
    assert!(
        report.cross_check.as_ref().unwrap().passed,
        "{:?}",
        report.cross_check
    );
}

#[test]
fn thermal_network_refuses_missing_interface_and_keeps_transient_unavailable_without_capacitance() {
    let mut missing = thermal_input();
    missing.case_to_sink = None;
    let refused = thermal_network(&missing, &context()).unwrap();
    assert_eq!(refused.status, ModelStatus::Refused);
    assert_eq!(
        refused.refusal.as_ref().unwrap().code,
        RefusalCode::MissingInterfaceResistance
    );

    let mut no_cap = thermal_input();
    no_cap.junction_to_case.capacitance = None;
    let report = thermal_network(&no_cap, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert!(!value.transient.is_known());
    assert!((value.steady_junction.value() - 110.0).abs() < 1e-9);
}

fn sn_curve() -> SnCurve {
    SnCurve {
        stress_coefficient: q(900.0, "MPa"),
        stress_exponent: -0.1,
        endurance_limit: Some(q(200.0, "MPa")),
        endurance_cycles: 1e6,
        yield_strength: q(500.0, "MPa"),
    }
}

fn strain_curve() -> StrainLifeCurve {
    StrainLifeCurve {
        stress_coefficient: q(900.0, "MPa"),
        stress_exponent: -0.1,
        ductility_coefficient: 0.5,
        ductility_exponent: -0.6,
        elastic_modulus: q(200.0, "GPa"),
        transition_cycles: 1e6,
        max_total_strain: 0.05,
    }
}

fn fatigue_input() -> FatigueInput {
    FatigueInput {
        blocks: vec![FatigueBlock {
            description: Some("service".to_string()),
            stress_amplitude: Some(q(300.0, "MPa")),
            strain_amplitude: None,
            mean_stress: q(0.0, "MPa"),
            cycles: 1000.0,
        }],
        uts: q(600.0, "MPa"),
        sn: Some(sn_curve()),
        strain_life: Some(strain_curve()),
        mean_stress_correction: MeanStressCorrection::Goodman,
    }
}

#[test]
fn fatigue_basquin_life_matches_closed_form() {
    let report = fatigue_damage(&fatigue_input(), &context()).unwrap();
    let value = report.value.as_known().unwrap();
    let expected_life = 0.5 * (300.0_f64 / 900.0).powf(-10.0);
    assert!((value.blocks[0].life_cycles.as_known().unwrap() - expected_life).abs() < 1e-6);
    assert!((value.damage - 1000.0 / expected_life).abs() < 1e-9);
    assert!(report.cross_check.as_ref().unwrap().passed);
}

#[test]
fn fatigue_goodman_mean_stress_correction_reduces_life() {
    let mut input = fatigue_input();
    input.blocks[0].mean_stress = q(100.0, "MPa");
    let report = fatigue_damage(&input, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    let expected = 0.5 * (360.0_f64 / 900.0).powf(-10.0);
    assert!((value.blocks[0].life_cycles.as_known().unwrap() - expected).abs() < 1e-6);
    assert!(value.damage > 0.2);
}

#[test]
fn fatigue_refuses_stress_above_yield_and_below_endurance_is_infinite_life() {
    let mut input = fatigue_input();
    input.blocks[0].stress_amplitude = Some(q(550.0, "MPa"));
    let refused = fatigue_damage(&input, &context()).unwrap();
    assert_eq!(refused.status, ModelStatus::Refused);
    assert_eq!(
        refused.refusal.as_ref().unwrap().code,
        RefusalCode::OutsideValidity
    );

    let mut infinite = fatigue_input();
    infinite.blocks[0].stress_amplitude = Some(q(100.0, "MPa"));
    let report = fatigue_damage(&infinite, &context()).unwrap();
    assert!(report.value.as_known().unwrap().infinite_life);
}

#[test]
fn fatigue_strain_life_is_bracketed_inside_its_transition() {
    let mut input = fatigue_input();
    input.blocks[0].stress_amplitude = None;
    input.blocks[0].strain_amplitude = Some(Quantity::dimensionless(0.01).unwrap());
    input.mean_stress_correction = MeanStressCorrection::Morrow;
    let report = fatigue_damage(&input, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    let life = value.blocks[0].life_cycles.as_known().unwrap();
    assert!((400.0..700.0).contains(life), "{life}");
    assert!(value.damage > 0.0);
}

#[test]
fn zero_stress_is_infinite_life_without_a_curve() {
    let mut input = fatigue_input();
    input.sn = None;
    input.blocks[0].stress_amplitude = Some(q(0.0, "MPa"));
    let report = fatigue_damage(&input, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert!(value.infinite_life);
    assert_eq!(value.damage, 0.0);
}

#[test]
fn fatigue_missing_curve_is_a_typed_missing_property() {
    let mut input = fatigue_input();
    input.sn = None;
    let report = fatigue_damage(&input, &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Refused);
    assert_eq!(
        report.refusal.as_ref().unwrap().code,
        RefusalCode::MissingProperty
    );
}

fn material(name: &str, grade: &str, temper: &str) -> MaterialRef {
    MaterialRef::new(
        name,
        RevisionId::new(3),
        Provenance::new(ProvenanceKind::ManufacturerData, "fixture material").unwrap(),
    )
    .unwrap()
    .with_grade(grade)
    .with_temper(temper)
}

fn property_record(
    id: &str,
    name: &str,
    grade: &str,
    temper: &str,
    thickness: (f64, f64),
    value: f64,
) -> MaterialPropertyRecord {
    let property_value = EngineeringValue::known(
        q(value, "MPa"),
        Provenance::new(ProvenanceKind::ManufacturerData, "fixture property").unwrap(),
        RevisionId::new(3),
    )
    .unwrap()
    .with_material(material(name, grade, temper))
    .with_thickness_range(ThicknessRange::new(q(thickness.0, "mm"), q(thickness.1, "mm")).unwrap())
    .with_applicability(
        Applicability::unrestricted().with_temperature(Interval::new(233.15, 373.15).unwrap()),
    );
    MaterialPropertyRecord {
        id: id.to_string(),
        property: "yield_strength".to_string(),
        value: property_value,
    }
}

fn material_query(grade: &str, temper: &str, thickness: f64) -> MaterialPropertyQuery {
    MaterialPropertyQuery {
        material: material("aluminium", grade, temper),
        property: "yield_strength".to_string(),
        thickness: Some(q(thickness, "mm")),
        temperature: Some(q(300.0, "K")),
        orientation: None,
        conditions: Vec::new(),
    }
}

#[test]
fn material_property_requires_matching_temper_and_thickness() {
    let records = vec![
        property_record("thin-t6", "aluminium", "6061", "T6", (0.0, 12.0), 276.0),
        property_record(
            "plate-t651",
            "aluminium",
            "6061",
            "T651",
            (50.0, 100.0),
            250.0,
        ),
    ];
    let report =
        lookup_material_property(&material_query("6061", "T651", 80.0), &records, &context())
            .unwrap();
    let value = report.value.as_known().unwrap();
    assert_eq!(value.value.quantity.as_known().unwrap().value(), 250.0);
    assert_eq!(value.matched_records, vec!["plate-t651"]);

    let refused =
        lookup_material_property(&material_query("6061", "T6", 80.0), &records, &context())
            .unwrap();
    assert_eq!(refused.status, ModelStatus::Refused);
    assert_eq!(
        refused.refusal.as_ref().unwrap().code,
        RefusalCode::OutsideValidity
    );

    let unavailable =
        lookup_material_property(&material_query("2024", "T4", 20.0), &records, &context())
            .unwrap();
    assert_eq!(unavailable.status, ModelStatus::Unavailable);
}

#[test]
fn material_property_outside_the_temperature_envelope_is_refused() {
    let records = vec![property_record(
        "plate-t651",
        "aluminium",
        "6061",
        "T651",
        (50.0, 100.0),
        250.0,
    )];
    let mut query = material_query("6061", "T651", 80.0);
    query.temperature = Some(q(400.0, "K"));
    let report = lookup_material_property(&query, &records, &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Refused);
    assert_eq!(
        report.refusal.as_ref().unwrap().code,
        RefusalCode::OutsideValidity
    );

    let mut missing = material_query("6061", "T651", 80.0);
    missing.temperature = None;
    let report = lookup_material_property(&missing, &records, &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Refused);
}

#[test]
fn conflicting_material_records_are_ambiguous_not_averaged() {
    let records = vec![
        property_record("plate-a", "aluminium", "6061", "T651", (50.0, 100.0), 250.0),
        property_record("plate-b", "aluminium", "6061", "T651", (50.0, 100.0), 260.0),
    ];
    let report =
        lookup_material_property(&material_query("6061", "T651", 80.0), &records, &context())
            .unwrap();
    assert_eq!(report.status, ModelStatus::Refused);
    assert_eq!(
        report.refusal.as_ref().unwrap().code,
        RefusalCode::AmbiguousProperty
    );
}

fn emc_input() -> EmcPcbInput {
    EmcPcbInput {
        stackup: Some(PcbStackup {
            dielectric_height: q(0.2, "mm"),
            copper_thickness: Some(q(35.0, "um")),
        }),
        trace_width: Some(q(0.3, "mm")),
        loop_length: None,
        capacitor_distance: Some(q(10.0, "mm")),
        via_length: Some(q(1.0, "mm")),
        frequency: Some(q(100.0, "MHz")),
        max_loop_inductance: Some(q(10.0, "nH")),
        actual_clearance: Some(q(0.5, "mm")),
        actual_creepage: Some(q(0.5, "mm")),
        rules: Some(EmcRules {
            min_clearance: q(0.6, "mm"),
            min_creepage: q(0.6, "mm"),
            provenance: Provenance::new(ProvenanceKind::Standard, "fixture rule").unwrap(),
        }),
    }
}

#[test]
fn emc_loop_inductance_and_placement_are_estimated_from_the_stackup() {
    let report = emc_pcb_check(&emc_input(), &context()).unwrap();
    let value = report.value.as_known().unwrap();
    let inductance = value.loop_inductance.as_known().unwrap().value();
    assert!((inductance - 1.843_067_690_106_012e-8).abs() < 1e-20);
    assert!((value.placement_inductance_factor.as_known().unwrap() - 11.0).abs() < 1e-9);
    assert!(!value.clearance_check.as_known().unwrap().pass);
    assert!(!value.creepage_check.as_known().unwrap().pass);
    assert!((value.impedance_at_frequency.as_known().unwrap().value() - 11.58).abs() < 0.05);
}

#[test]
fn emc_missing_stackup_is_typed_unavailable() {
    let mut input = emc_input();
    input.stackup = None;
    let report = emc_pcb_check(&input, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert!(!value.loop_inductance.is_known());
    assert!(!value.impedance_at_frequency.is_known());
}

#[test]
fn emc_rules_without_actual_spacing_are_unavailable() {
    let mut input = emc_input();
    input.actual_clearance = None;
    let report = emc_pcb_check(&input, &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert!(!value.clearance_check.is_known());
}
