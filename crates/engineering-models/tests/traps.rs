use engineering_core::{Provenance, ProvenanceKind, RevisionId, ValueState};
use engineering_knowledge::KnowledgeRegistry;
use engineering_models::{
    bolt::{torque_preload, NutFactorScatter, TorquePreloadInput},
    molding::{check_moldability, MoldabilityInput, MoldingRules},
    mosfet::{mosfet_conduction_thermal, Estimate, MosfetThermalInput},
    ModelContext, ModelStatus, RefusalCode,
};
use engineering_units::{Dimension, Distribution, Interval, Quantity, Uncertainty, UnitRegistry};

fn context() -> ModelContext {
    ModelContext::new(
        RevisionId::new(1),
        Provenance::new(ProvenanceKind::Specification, "trap test fixture").unwrap(),
    )
}

fn bolt_input(torque: Quantity, k: NutFactorScatter) -> TorquePreloadInput {
    let units = UnitRegistry::si();
    TorquePreloadInput {
        torque,
        nominal_diameter: Quantity::new(12.0, units.require("mm").unwrap()).unwrap(),
        thread_pitch: Some(Quantity::new(1.75, units.require("mm").unwrap()).unwrap()),
        tensile_area: None,
        nut_factor: k,
        proof_strength: ValueState::known(
            Quantity::new(640.0, units.require("MPa").unwrap()).unwrap(),
        ),
    }
}

fn mosfet_input() -> MosfetThermalInput {
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
fn trap_torque_preload_with_k_scatter_has_a_hard_interval() {
    let units = UnitRegistry::si();
    let torque = Quantity::new(100.0, units.require("N*m").unwrap()).unwrap();
    let scatter = NutFactorScatter::new(
        0.2,
        Uncertainty::Interval(Interval::new(0.16, 0.24).unwrap()),
        Interval::new(0.1, 0.3).unwrap(),
    )
    .unwrap();
    let report = torque_preload(&bolt_input(torque, scatter), &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Solved);
    let value = report.value.as_known().unwrap();
    let nominal = value.preload_nominal.value();
    let lower = value.preload_bounds.as_ref().unwrap().min.value();
    let upper = value.preload_bounds.as_ref().unwrap().max.value();
    assert!((nominal - 41_666.666_666_666_664).abs() < 1e-6);
    assert!((lower - 34_722.222_222_222_22).abs() < 1e-6);
    assert!((upper - 52_083.333_333_333_33).abs() < 1e-6);
    assert!(lower < nominal && nominal < upper);
    assert!(report.receipt.executed);
    assert!(report.cross_check.as_ref().unwrap().passed);
}

#[test]
fn trap_unrealistic_k_is_refused_instead_of_producing_preload() {
    let units = UnitRegistry::si();
    let torque = Quantity::new(100.0, units.require("N*m").unwrap()).unwrap();
    let scatter = NutFactorScatter::new(0.02, Uncertainty::Exact, Interval::new(0.1, 0.3).unwrap());
    assert!(
        scatter.is_err(),
        "the nominal must lie inside its validity range"
    );

    let input = bolt_input(
        torque,
        NutFactorScatter::new(
            0.2,
            Uncertainty::Distribution(Distribution::uniform(0.01, 0.02).unwrap()),
            Interval::new(0.1, 0.3).unwrap(),
        )
        .unwrap(),
    );
    let report = torque_preload(&input, &context()).unwrap();
    assert_eq!(report.status, ModelStatus::Refused);
    assert_eq!(
        report.refusal.as_ref().unwrap().code,
        RefusalCode::OutsideValidity
    );
    assert!(!report.value.is_known());
}

#[test]
fn trap_mosfet_thermal_chain_uses_coupled_temperature_and_refuses_missing_interface() {
    let report = mosfet_conduction_thermal(&mosfet_input(), &context()).unwrap();
    let value = report.value.as_known().unwrap();
    assert!((value.conduction_loss.value() - 4.132_231_404_958_68).abs() < 1e-9);
    assert!((value.junction_temperature.value() - 33.264_462_809_917_4).abs() < 1e-8);
    assert!(value.stability_denominator > 0.0);
    assert!(report.receipt.convergence.converged);
    assert!(report.receipt.convergence.residual <= 1e-9);
    assert!(report.cross_check.as_ref().unwrap().passed);

    let mut missing_interface = mosfet_input();
    missing_interface.r_theta_cs = None;
    let refused = mosfet_conduction_thermal(&missing_interface, &context()).unwrap();
    assert_eq!(refused.status, ModelStatus::Refused);
    assert_eq!(
        refused.refusal.as_ref().unwrap().code,
        RefusalCode::MissingInterfaceResistance
    );
}

#[test]
fn trap_mosfet_thermal_runaway_is_a_typed_refusal() {
    let units = UnitRegistry::si();
    let mut input = mosfet_input();
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

#[test]
fn trap_injection_mold_draft_and_wall_geometry_cannot_be_manufactured() {
    let units = UnitRegistry::si();
    let rules = MoldingRules::new(
        Quantity::new(1.0, units.require("deg").unwrap()).unwrap(),
        0.25,
        Some(100.0),
        Provenance::new(ProvenanceKind::EngineeringPractice, "DFM review").unwrap(),
    )
    .unwrap();
    let report = check_moldability(
        &MoldabilityInput {
            nominal_wall: Quantity::new(2.5, units.require("mm").unwrap()).unwrap(),
            min_wall: Quantity::new(2.2, units.require("mm").unwrap()).unwrap(),
            max_wall: Quantity::new(2.9, units.require("mm").unwrap()).unwrap(),
            draft_actual: Quantity::new(0.5, units.require("deg").unwrap()).unwrap(),
            flow_length: Some(Quantity::new(120.0, units.require("mm").unwrap()).unwrap()),
            rules,
        },
        &context(),
    )
    .unwrap();
    let value = report.value.as_known().unwrap();
    assert!(!value.passes_all_required);
    assert_eq!(
        value.checks[0].status,
        engineering_models::CheckStatus::Fail
    );
    assert_eq!(
        value.checks[1].status,
        engineering_models::CheckStatus::Fail
    );
    assert!((value.wall_variation - 0.28).abs() < 1e-12);
    assert!((value.draft_margin.value() + 0.5).abs() < 1e-12);
    assert!((value.flow_ratio.unwrap() - 48.0).abs() < 1e-12);
}

#[test]
fn trap_unit_conversion_changes_preload_by_35_percent() {
    let units = UnitRegistry::si();
    let imperial = Quantity::new(100.0, units.require("lbf*ft").unwrap()).unwrap();
    let metric = imperial.to_unit(units.require("N*m").unwrap()).unwrap();
    assert!((metric.value() - 135.581_794_833_140_04).abs() < 1e-9);

    let scatter = || {
        NutFactorScatter::new(0.2, Uncertainty::Exact, Interval::new(0.1, 0.3).unwrap()).unwrap()
    };
    let correct = torque_preload(&bolt_input(metric, scatter()), &context()).unwrap();
    let wrong = torque_preload(
        &bolt_input(
            Quantity::new(100.0, units.require("N*m").unwrap()).unwrap(),
            scatter(),
        ),
        &context(),
    )
    .unwrap();
    let correct_f = correct.value.as_known().unwrap().preload_nominal.value();
    let wrong_f = wrong.value.as_known().unwrap().preload_nominal.value();
    let ratio = correct_f / wrong_f;
    assert!((ratio - 1.355_817_948_331_400_4).abs() < 1e-12);
    assert!(ratio > 1.35);
}

#[test]
fn trap_wrong_dimension_is_refused() {
    let units = UnitRegistry::si();
    let input = bolt_input(
        Quantity::new(100.0, units.require("N").unwrap()).unwrap(),
        NutFactorScatter::new(0.2, Uncertainty::Exact, Interval::new(0.1, 0.3).unwrap()).unwrap(),
    );
    let error = torque_preload(&input, &context()).unwrap_err();
    assert!(matches!(
        error,
        engineering_models::ModelError::DimensionMismatch {
            field: "torque",
            expected: Dimension::ENERGY,
            ..
        }
    ));
}

#[test]
fn trap_uncertainty_is_deterministic_and_not_averaged_away() {
    let units = UnitRegistry::si();
    let scatter = || {
        NutFactorScatter::new(
            0.2,
            Uncertainty::Distribution(Distribution::uniform(0.18, 0.22).unwrap()),
            Interval::new(0.1, 0.3).unwrap(),
        )
        .unwrap()
    };
    let first = torque_preload(
        &bolt_input(
            Quantity::new(100.0, units.require("N*m").unwrap()).unwrap(),
            scatter(),
        ),
        &context(),
    )
    .unwrap();
    let second = torque_preload(
        &bolt_input(
            Quantity::new(100.0, units.require("N*m").unwrap()).unwrap(),
            scatter(),
        ),
        &context(),
    )
    .unwrap();
    let first_value = first.value.as_known().unwrap();
    let second_value = second.value.as_known().unwrap();
    assert!(first_value.preload_std_dev.is_some());
    assert_eq!(
        first_value.preload_std_dev.as_ref().unwrap().value(),
        second_value.preload_std_dev.as_ref().unwrap().value()
    );
    let bounds = first_value.preload_bounds.as_ref().unwrap();
    assert!(bounds.min.value() < bounds.max.value());
    assert!(!first.uncertainty.is_exact());
}

#[test]
fn knowledge_pack_worked_cases_match_executed_models() {
    let registry = KnowledgeRegistry::bundled().unwrap();
    let units = UnitRegistry::si();

    let bolt_pack = registry.get("bolted-joint", "1.0.0").unwrap();
    let bolt_case = &bolt_pack.worked_cases[0];
    let torque = Quantity::new(
        bolt_case.inputs[0].value,
        units.require(&bolt_case.inputs[0].unit).unwrap(),
    )
    .unwrap();
    let k_value = bolt_case.inputs[1].value;
    let diameter = Quantity::new(
        bolt_case.inputs[2].value,
        units.require(&bolt_case.inputs[2].unit).unwrap(),
    )
    .unwrap();
    let mut input = bolt_input(
        torque,
        NutFactorScatter::new(
            k_value,
            Uncertainty::Exact,
            Interval::new(0.1, 0.3).unwrap(),
        )
        .unwrap(),
    );
    input.nominal_diameter = diameter;
    input.thread_pitch = None;
    let report = torque_preload(&input, &context()).unwrap();
    let expected = &bolt_case.expected[0];
    let actual = report.value.as_known().unwrap().preload_nominal.value();
    assert!((actual - expected.value).abs() <= expected.tolerance.max(1e-6));

    let mosfet_pack = registry.get("power-electronics", "1.0.0").unwrap();
    let mosfet_case = &mosfet_pack.worked_cases[0];
    let expected_loss = &mosfet_case.expected[0];
    let report = mosfet_conduction_thermal(&mosfet_input(), &context()).unwrap();
    let actual_loss = report.value.as_known().unwrap().conduction_loss.value();
    assert!((actual_loss - expected_loss.value).abs() <= expected_loss.tolerance);

    let molding_pack = registry.get("injection-molding", "1.0.0").unwrap();
    let molding_case = &molding_pack.worked_cases[0];
    let expected_variation = &molding_case.expected[0];
    let rules = MoldingRules::new(
        Quantity::new(1.0, units.require("deg").unwrap()).unwrap(),
        0.25,
        Some(100.0),
        Provenance::new(ProvenanceKind::EngineeringPractice, "DFM review").unwrap(),
    )
    .unwrap();
    let report = check_moldability(
        &MoldabilityInput {
            nominal_wall: Quantity::new(
                molding_case.inputs[0].value,
                units.require(&molding_case.inputs[0].unit).unwrap(),
            )
            .unwrap(),
            min_wall: Quantity::new(
                molding_case.inputs[1].value,
                units.require(&molding_case.inputs[1].unit).unwrap(),
            )
            .unwrap(),
            max_wall: Quantity::new(
                molding_case.inputs[2].value,
                units.require(&molding_case.inputs[2].unit).unwrap(),
            )
            .unwrap(),
            draft_actual: Quantity::new(
                molding_case.inputs[3].value,
                units.require(&molding_case.inputs[3].unit).unwrap(),
            )
            .unwrap(),
            flow_length: Some(
                Quantity::new(
                    molding_case.inputs[5].value,
                    units.require(&molding_case.inputs[5].unit).unwrap(),
                )
                .unwrap(),
            ),
            rules,
        },
        &context(),
    )
    .unwrap();
    let actual_variation = report.value.as_known().unwrap().wall_variation;
    assert!((actual_variation - expected_variation.value).abs() <= expected_variation.tolerance);
}
