use engineering_units::UnitRegistry;

use super::{case_error, falsifiable};
use crate::{CaseBuilder, CaseOutcome, CaseReport, ExpectedKind, Metrics, Observation};

pub(crate) const UNIT_ID: &str = "units/torque-lbf-ft";

pub(crate) fn unit_conversion() -> Result<CaseReport, crate::BenchError> {
    let units = UnitRegistry::si();
    let lbf_ft = units
        .require("lbf*ft")
        .map_err(|error| case_error(UNIT_ID, error))?;
    let newton_metre = units
        .require("N*m")
        .map_err(|error| case_error(UNIT_ID, error))?;
    let torque = engineering_units::Quantity::new(100.0, lbf_ft.clone())
        .map_err(|error| case_error(UNIT_ID, error))?;
    let converted = torque
        .to_unit(newton_metre.clone())
        .map_err(|error| case_error(UNIT_ID, error))?;
    let back = converted
        .to_unit(lbf_ft.clone())
        .map_err(|error| case_error(UNIT_ID, error))?;
    let expected = 100.0 * 4.448_221_615_260_5 * 0.304_8;
    let relative_error = ((converted.value() - expected) / expected).abs();
    let round_trip_error = (back.value() - 100.0).abs();
    let shortfall = (expected - 100.0) / expected;
    let incompatible = engineering_units::Quantity::new(
        100.0,
        units
            .require("N")
            .map_err(|error| case_error(UNIT_ID, error))?,
    )
    .map_err(|error| case_error(UNIT_ID, error))?;
    let dimension_refused = torque.add(&incompatible).is_err();
    let unknown_symbol_refused = units.require("lbf-ft").is_err();
    let physics = relative_error < 1e-12
        && round_trip_error < 1e-9
        && shortfall > 0.25
        && dimension_refused
        && unknown_symbol_refused;
    let test = falsifiable(
        "100 lbf*ft is the same torque as 100 N*m.",
        "Compare the torque tool against a deadweight/lever primary standard or a calibrated torque transducer in both unit settings; convert the indicated values with exact SI definitions.",
        "The two readings agree after conversion within the tool's declared tolerance and the dimensional check refuses force/energy mixing.",
        "The converted values differ beyond tolerance, or the tool treats lbf*ft and N*m as interchangeable numbers.",
        "Deadweight/lever primary standard or calibrated torque transducer with traceable certificate.",
        &[
            "1 lbf = 4.4482216152605 N and 1 ft = 0.3048 m exactly by definition",
            "100 lbf*ft = 135.58179483314 N*m, a 26.2% difference from the shallow 100 N*m answer",
        ],
    );
    CaseBuilder::new(UNIT_ID, "units", "lbf*ft and N*m are not interchangeable numbers", ExpectedKind::Solved)
        .shallow("100 ft-lb equals 100 N-m; the number is the same.")
        .outcome(CaseOutcome::Solved {
            observations: vec![
                Observation::quantity("100 lbf*ft in N*m", converted.value(), "N*m"),
                Observation::quantity("round trip in lbf*ft", back.value(), "lbf*ft"),
                Observation::quantity("shallow 100 N*m shortfall fraction", shortfall, "1"),
            ],
            notes: vec![
                format!("relative conversion error {relative_error:.3e}"),
                format!("round-trip error {round_trip_error:.3e}"),
                format!("dimensional mismatch refused: {dimension_refused}"),
                format!("unknown unit symbol refused: {unknown_symbol_refused}"),
            ],
        })
        .metrics(
            Metrics::default()
                .with_physics(physics)
                .with_authority(true)
                .with_missing_info(unknown_symbol_refused)
                .with_boundaries(dimension_refused)
                .with_uncertainty(true)
                .with_trap(shortfall > 0.25)
                .with_experiment(true)
                .with_refusal(dimension_refused && unknown_symbol_refused),
        )
        .evidence(vec![
            "unit registry scales: lbf = 4.4482216152605 N, ft = 0.3048 m".to_string(),
            format!("UnitRegistry::convert(100, \"lbf*ft\", \"N*m\") = {:.12}", units.convert(100.0, "lbf*ft", "N*m").map_err(|error| case_error(UNIT_ID, error))?),
            "the registry refuses unknown symbols and dimensionally incompatible arithmetic".to_string(),
        ])
        .missing(vec![
            "a torque standard and calibration certificate for the actual instrument".to_string(),
        ])
        .boundaries(vec![
            "conversion is only valid between units of the same dimension (energy/torque)".to_string(),
            "affine temperature units are not multiplied or divided".to_string(),
        ])
        .uncertainty("Unit conversion is exact by definition; the remaining uncertainty belongs to the instrument and calibration, not to the arithmetic.")
        .test(test)
        .build()
}
