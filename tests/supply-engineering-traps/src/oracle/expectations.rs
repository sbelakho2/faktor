//! Oracle-owned expectations. None of these values is read from the SUT.

use engineering_core::GateId;

use super::{
    AuthorityExpectation, ControlExpectation, MoneyWindow, OracleExpectation, QuantityWindow,
    TrapExpectation, UncertaintyExpectation,
};
use crate::contract::{ExpectedKind, GateStateKind};

pub const CASE_COUNT: usize = 15;

pub const CASE_IDS: &[&str] = &[
    "analog/opamp-nominal-gain-stability",
    "analog/opamp-large-signal-slew",
    "power/flyback-leakage-overvoltage",
    "pcb/alternate-thermal-pad",
    "materials/aluminium-temper-thickness",
    "machine/fastener-nut-factor-scatter",
    "fatigue/sn-outside-regime",
    "thermal/missing-contact-resistance",
    "manufacturing/molding-draft-wall",
    "fea/mesh-convergence-pathology",
    "supply/cheaper-supplier-tco",
    "customs/hs-classification-ambiguity",
    "units/torque-lbf-ft",
    "supply/unknown-cost-not-zero",
    "emc/return-path-loop-inductance",
];

const NO_WINDOWS: &[QuantityWindow] = &[];
const NO_MONEY: &[MoneyWindow] = &[];
const NO_ABSENT: &[&str] = &[];
const NO_GROUPS: &[&[&str]] = &[];
const NO_GATES: &[(GateId, GateStateKind)] = &[];
const NO_CONTROLS: &[ControlExpectation] = &[];

const OPAMP: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Rejected,
    refusal_codes: &["PhaseMarginViolation"],
    windows: &[
        QuantityWindow {
            name: "phase_margin",
            lower: -40.0,
            upper: -20.0,
        },
        QuantityWindow {
            name: "gain_margin",
            lower: -20.0,
            upper: -10.0,
        },
        QuantityWindow {
            name: "required_noise_gain",
            lower: 500.0,
            upper: 600.0,
        },
    ],
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::PackItems(&[
        ("analog-stability", "prop-opamp-loop"),
        ("analog-stability", "rule-phase-margin"),
        ("analog-stability", "trap-nominal-gain-stability"),
        ("analog-stability", "model-opamp-loop-stability"),
    ]),
    missing: &[
        &["lot", "variation"],
        &["PCB", "parasitic"],
        &["large-signal", "slew"],
        &["saturation", "output"],
    ],
    boundaries: &[
        &["phase margin"],
        &["gain margin"],
        &["second pole", "first pole"],
    ],
    uncertainty: UncertaintyExpectation::Exact,
    trap: TrapExpectation {
        requires_non_solved: true,
        quantity_outside: Some(QuantityWindow {
            name: "phase_margin",
            lower: 0.0,
            upper: 90.0,
        }),
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["phase margin"],
        &["load", "capacitance"],
        &["oscillation", "overshoot"],
    ],
    gates: &[
        (GateId::Theory, GateStateKind::Established),
        (GateId::Validity, GateStateKind::Established),
        (GateId::ExecutedAnalysis, GateStateKind::Established),
        (GateId::CrossCheck, GateStateKind::Established),
        (GateId::Verification, GateStateKind::Established),
    ],
    controls: &[
        ControlExpectation {
            name: "compensated",
            kind: ExpectedKind::Solved,
            refusal_code: None,
            uncertainty: Some(UncertaintyExpectation::Exact),
        },
        ControlExpectation {
            name: "unordered-poles",
            kind: ExpectedKind::Refused,
            refusal_code: Some("OutsideValidity"),
            uncertainty: Some(UncertaintyExpectation::NotEstimated),
        },
    ],
    coverage: &[
        &["prop-opamp-loop"],
        &["rule-phase-margin"],
        &["trap-nominal-gain-stability"],
    ],
};

const LARGE_SIGNAL: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::ResearchRequired,
    refusal_codes: NO_ABSENT,
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::PackText {
        pack_id: "analog-stability",
        item_id: "val-linear-small-signal",
        tokens: &["large-signal", "slew"],
    },
    missing: &[
        &["large-signal", "slew"],
        &["saturation"],
        &["thermal drift", "temperature"],
        &["device", "lot"],
    ],
    boundaries: &[&["small-signal", "linear"], &["large-signal", "slew"]],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["slew", "large-signal"],
        &["step", "oscilloscope"],
        &["saturation"],
    ],
    gates: &[
        (GateId::Authority, GateStateKind::Established),
        (GateId::Theory, GateStateKind::ResearchRequired),
        (GateId::Validity, GateStateKind::ResearchRequired),
    ],
    controls: NO_CONTROLS,
    coverage: NO_GROUPS,
};

const FLYBACK: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::ResearchRequired,
    refusal_codes: NO_ABSENT,
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::PackText {
        pack_id: "flyback-converters",
        item_id: "val-clamp-above-reflected",
        tokens: &["avalanche"],
    },
    missing: &[
        &["leakage"],
        &["turn-off", "switching waveform"],
        &["snubber", "clamp"],
        &["avalanche"],
        &["high-line", "temperature"],
    ],
    boundaries: &[
        &["vds", "drain-source"],
        &["avalanche"],
        &["reflected voltage"],
    ],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["vds", "drain-source"],
        &["leakage", "avalanche"],
        &["oscilloscope", "probe"],
    ],
    gates: &[
        (GateId::Authority, GateStateKind::Established),
        (GateId::Theory, GateStateKind::ResearchRequired),
        (GateId::Validity, GateStateKind::ResearchRequired),
    ],
    controls: NO_CONTROLS,
    coverage: NO_GROUPS,
};

const PCB: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Refused,
    refusal_codes: &["MissingInterfaceResistance"],
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::PackItems(&[
        ("power-electronics", "prop-contact-rth"),
        ("power-electronics", "rule-contact-resistance"),
        ("power-electronics", "model-conduction-thermal"),
    ]),
    missing: &[
        &["pad geometry", "thermal-pad"],
        &["interface material", "mounting pressure"],
        &["case-to-sink", "interface resistance"],
        &["airflow", "enclosure"],
    ],
    boundaries: &[&["interface resistance"], &["junction temperature"]],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["interface resistance"],
        &["junction temperature"],
        &["thermal", "jesd51"],
    ],
    gates: &[
        (GateId::Theory, GateStateKind::Established),
        (GateId::Validity, GateStateKind::Established),
        (GateId::ExecutedAnalysis, GateStateKind::Unavailable),
        (GateId::Verification, GateStateKind::Established),
    ],
    controls: &[
        ControlExpectation {
            name: "original-with-contact",
            kind: ExpectedKind::Solved,
            refusal_code: None,
            uncertainty: Some(UncertaintyExpectation::Interval),
        },
        ControlExpectation {
            name: "zero-interface",
            kind: ExpectedKind::Refused,
            refusal_code: Some("NonPositiveGeometry"),
            uncertainty: Some(UncertaintyExpectation::NotEstimated),
        },
    ],
    coverage: &[&["prop-contact-rth"], &["rule-contact-resistance"]],
};

const ALUMINIUM: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::ResearchRequired,
    refusal_codes: NO_ABSENT,
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::NoPackCoverage(&[
        &["plate-t651"],
        &["yield_strength"],
        &["250 mpa"],
    ]),
    missing: &[
        &["property record", "revision-bound"],
        &["temper"],
        &["thickness", "plate"],
        &["specification", "astm"],
        &["scatter", "uncertainty"],
    ],
    boundaries: &[
        &["temper"],
        &["thickness", "plate"],
        &["orientation", "test direction"],
    ],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["tensile"],
        &["temper", "thickness"],
        &["mill certificate", "astm"],
    ],
    gates: &[
        (GateId::Authority, GateStateKind::Established),
        (GateId::Theory, GateStateKind::ResearchRequired),
        (GateId::Validity, GateStateKind::ResearchRequired),
    ],
    controls: NO_CONTROLS,
    coverage: NO_GROUPS,
};

const EMC: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Rejected,
    refusal_codes: &["ReturnPathViolation"],
    windows: &[
        QuantityWindow {
            name: "loop_inductance",
            lower: 1.7e-8,
            upper: 2.0e-8,
        },
        QuantityWindow {
            name: "placement_factor",
            lower: 10.5,
            upper: 11.5,
        },
        QuantityWindow {
            name: "clearance_margin",
            lower: -0.11,
            upper: -0.09,
        },
        QuantityWindow {
            name: "creepage_margin",
            lower: -0.11,
            upper: -0.09,
        },
    ],
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::PackItems(&[
        ("emc-pcb", "prop-stackup"),
        ("emc-pcb", "rule-decoupling-distance"),
        ("emc-pcb", "rule-spacing"),
        ("emc-pcb", "trap-same-copper-area"),
        ("emc-pcb", "model-pcb-loop-inductance"),
    ]),
    missing: &[
        &["stackup", "dielectric"],
        &["placement", "via"],
        &["creepage", "clearance"],
        &["plane split", "breakout"],
    ],
    boundaries: &[
        &["stackup", "dielectric"],
        &["placement", "loop"],
        &["clearance", "creepage"],
    ],
    uncertainty: UncertaintyExpectation::Exact,
    trap: TrapExpectation {
        requires_non_solved: true,
        quantity_outside: Some(QuantityWindow {
            name: "loop_inductance",
            lower: 0.0,
            upper: 1.0e-8,
        }),
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["loop", "impedance"],
        &["placement", "capacitor"],
        &["clearance", "creepage"],
    ],
    gates: &[
        (GateId::Theory, GateStateKind::Established),
        (GateId::Validity, GateStateKind::Established),
        (GateId::ExecutedAnalysis, GateStateKind::Established),
        (GateId::Verification, GateStateKind::Established),
    ],
    controls: &[ControlExpectation {
        name: "adjacent-placement",
        kind: ExpectedKind::Solved,
        refusal_code: None,
        uncertainty: Some(UncertaintyExpectation::Exact),
    }],
    coverage: &[
        &["prop-stackup"],
        &["rule-decoupling-distance"],
        &["trap-same-copper-area"],
    ],
};

const FASTENER: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Refused,
    refusal_codes: &["OutsideValidity"],
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::PackItems(&[
        ("bolted-joint", "prop-nut-factor"),
        ("bolted-joint", "val-k-domain"),
        ("bolted-joint", "trap-unrealistic-k"),
    ]),
    missing: &[
        &["nut-factor", "k distribution"],
        &["lubrication", "coating", "thread"],
        &["torque-tension"],
    ],
    boundaries: &[
        &["0.12", "0.28", "validity"],
        &["utilization", "proof"],
        &["preload", "bounded"],
    ],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[&["torque-tension"], &["nut factor"], &["preload"]],
    gates: &[
        (GateId::Theory, GateStateKind::Established),
        (GateId::Validity, GateStateKind::Established),
        (GateId::ExecutedAnalysis, GateStateKind::Unavailable),
        (GateId::Verification, GateStateKind::Established),
    ],
    controls: &[
        ControlExpectation {
            name: "valid-scatter",
            kind: ExpectedKind::Solved,
            refusal_code: None,
            uncertainty: Some(UncertaintyExpectation::Interval),
        },
        ControlExpectation {
            name: "invalid-scatter",
            kind: ExpectedKind::Refused,
            refusal_code: Some("OutsideValidity"),
            uncertainty: Some(UncertaintyExpectation::NotEstimated),
        },
    ],
    coverage: &[&["prop-nut-factor"], &["val-k-domain"]],
};

const FATIGUE: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::ResearchRequired,
    refusal_codes: NO_ABSENT,
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::NoPackCoverage(&[
        &["stress_coefficient"],
        &["endurance_limit"],
        &["sn_curve"],
    ]),
    missing: &[
        &["s-n", "stress ratio"],
        &["surface", "notch"],
        &["environment", "temperature"],
        &["cycle", "run-out"],
        &["scatter", "statistical"],
    ],
    boundaries: &[
        &["stress ratio"],
        &["cycle", "run-out"],
        &["endurance limit"],
    ],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[&["fatigue"], &["stress ratio"], &["run-out", "endurance"]],
    gates: &[
        (GateId::Authority, GateStateKind::Established),
        (GateId::Theory, GateStateKind::ResearchRequired),
        (GateId::Validity, GateStateKind::ResearchRequired),
    ],
    controls: NO_CONTROLS,
    coverage: NO_GROUPS,
};

const THERMAL: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Refused,
    refusal_codes: &["MissingInterfaceResistance"],
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: &["junction_temperature"],
    authority: AuthorityExpectation::PackItems(&[
        ("power-electronics", "prop-contact-rth"),
        ("power-electronics", "rule-contact-resistance"),
        ("power-electronics", "trap-missing-contact-resistance"),
    ]),
    missing: &[
        &["interface material", "thermal interface"],
        &["case-to-sink", "interface resistance"],
    ],
    boundaries: &[
        &["strictly positive", "positive"],
        &["junction temperature"],
    ],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["interface resistance"],
        &["junction temperature"],
        &["mounting"],
    ],
    gates: &[
        (GateId::Theory, GateStateKind::Established),
        (GateId::Validity, GateStateKind::Established),
        (GateId::ExecutedAnalysis, GateStateKind::Unavailable),
        (GateId::Verification, GateStateKind::Established),
    ],
    controls: &[
        ControlExpectation {
            name: "with-contact",
            kind: ExpectedKind::Solved,
            refusal_code: None,
            uncertainty: Some(UncertaintyExpectation::Interval),
        },
        ControlExpectation {
            name: "zero-interface",
            kind: ExpectedKind::Refused,
            refusal_code: Some("NonPositiveGeometry"),
            uncertainty: Some(UncertaintyExpectation::NotEstimated),
        },
    ],
    coverage: &[
        &["prop-contact-rth"],
        &["rule-contact-resistance"],
        &["trap-missing-contact-resistance"],
    ],
};

const MOLDING: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Rejected,
    refusal_codes: &["MoldabilityViolation"],
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::PackItems(&[
        ("injection-molding", "rule-draft"),
        ("injection-molding", "rule-wall-uniformity"),
        ("injection-molding", "rule-flow-ratio"),
        ("injection-molding", "trap-draft-units"),
    ]),
    missing: &[&["draft"], &["wall"], &["texture", "ejection"]],
    boundaries: &[
        &["draft_actual", "draft"],
        &["wall_variation", "wall"],
        &["flow_ratio", "flow"],
    ],
    uncertainty: UncertaintyExpectation::Exact,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[&["draft"], &["wall"], &["flow"]],
    gates: &[
        (GateId::Theory, GateStateKind::Established),
        (GateId::Validity, GateStateKind::Established),
        (GateId::ExecutedAnalysis, GateStateKind::Established),
        (GateId::CrossCheck, GateStateKind::Established),
        (GateId::Verification, GateStateKind::Established),
    ],
    controls: &[
        ControlExpectation {
            name: "radian-control",
            kind: ExpectedKind::Solved,
            refusal_code: None,
            uncertainty: Some(UncertaintyExpectation::Exact),
        },
        ControlExpectation {
            name: "good-control",
            kind: ExpectedKind::Solved,
            refusal_code: None,
            uncertainty: Some(UncertaintyExpectation::Exact),
        },
    ],
    coverage: &[
        &["rule-draft"],
        &["rule-wall-uniformity"],
        &["rule-flow-ratio"],
        &["trap-draft-units"],
    ],
};

const FEA: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Refused,
    refusal_codes: &["MeshConvergenceNotEstablished"],
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: &["stress", "displacement"],
    authority: AuthorityExpectation::NoPackCoverage(&[
        &["structural solver"],
        &["convergence residual"],
    ]),
    missing: &[
        &["mesh-independence", "convergence"],
        &["element quality", "aspect ratio"],
        &["reference", "analytical"],
    ],
    boundaries: &[
        &["monotone"],
        &["observed order", "discretization"],
        &["bounded"],
    ],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["refine", "refinement"],
        &["convergence"],
        &["reference", "analytical"],
    ],
    gates: &[
        (GateId::Theory, GateStateKind::ResearchRequired),
        (GateId::ExecutedAnalysis, GateStateKind::Unavailable),
        (GateId::Verification, GateStateKind::Established),
    ],
    controls: NO_CONTROLS,
    coverage: &[&["mesh-only"], &["gmsh"]],
};

const SUPPLIER: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Solved,
    refusal_codes: NO_ABSENT,
    windows: NO_WINDOWS,
    money: &[
        MoneyWindow {
            name: "incumbent landed total",
            lower: 5_896_493_151,
            upper: 5_896_493_151,
        },
        MoneyWindow {
            name: "cheap landed total",
            lower: 6_963_708_904,
            upper: 6_963_708_904,
        },
        MoneyWindow {
            name: "price-only saving",
            lower: 750_000_000,
            upper: 750_000_000,
        },
        MoneyWindow {
            name: "landed cost difference",
            lower: 1_067_215_753,
            upper: 1_067_215_753,
        },
        MoneyWindow {
            name: "incumbent per unit",
            lower: 11_792_986,
            upper: 11_792_986,
        },
        MoneyWindow {
            name: "cheap per unit",
            lower: 13_927_418,
            upper: 13_927_418,
        },
    ],
    absent: NO_ABSENT,
    authority: AuthorityExpectation::AnswerCoverage,
    missing: &[
        &["binding quote", "quote"],
        &["freight", "duty"],
        &["yield", "quality"],
    ],
    boundaries: &[&["moq"], &["carrying"], &["cash cycle", "days"]],
    uncertainty: UncertaintyExpectation::Exact,
    trap: TrapExpectation {
        money_above: Some(("cheap per unit", 11_792_986)),
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["landed cost", "landed"],
        &["binding quotes", "quotes"],
        &["yield", "inspection"],
    ],
    gates: NO_GATES,
    controls: NO_CONTROLS,
    coverage: &[
        &["incumbent-price"],
        &["cheap-freight"],
        &["cheap-quality"],
        &["cheap-cash-cycle"],
    ],
};

const HS: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Ambiguous,
    refusal_codes: NO_ABSENT,
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::AnswerCoverage,
    missing: &[
        &["composition"],
        &["function"],
        &["essential character"],
        &["binding ruling"],
    ],
    boundaries: &[&["one", "single"], &["candidates", "viable"]],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[&["binding ruling"], &["composition", "function"], &["duty"]],
    gates: NO_GATES,
    controls: &[ControlExpectation {
        name: "missing-facts",
        kind: ExpectedKind::ResearchRequired,
        refusal_code: None,
        uncertainty: Some(UncertaintyExpectation::NotEstimated),
    }],
    coverage: &[&["853222"], &["853229"], &["binding"]],
};

const UNITS: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Solved,
    refusal_codes: NO_ABSENT,
    windows: &[
        QuantityWindow {
            name: "100 lbf*ft in N*m",
            lower: 135.581_794_833_1,
            upper: 135.581_794_833_2,
        },
        QuantityWindow {
            name: "round trip in lbf*ft",
            lower: 99.999_999_999,
            upper: 100.000_000_001,
        },
        QuantityWindow {
            name: "shallow 100 N*m shortfall fraction",
            lower: 0.262_43,
            upper: 0.262_44,
        },
    ],
    money: NO_MONEY,
    absent: NO_ABSENT,
    authority: AuthorityExpectation::AnswerCoverage,
    missing: &[&["calibration", "torque standard"]],
    boundaries: &[&["dimension"], &["temperature", "affine"]],
    uncertainty: UncertaintyExpectation::Exact,
    trap: TrapExpectation {
        quantity_outside: Some(QuantityWindow {
            name: "100 lbf*ft in N*m",
            lower: 90.0,
            upper: 110.0,
        }),
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["torque"],
        &["conversion", "unit"],
        &["calibrated", "traceable"],
    ],
    gates: NO_GATES,
    controls: &[
        ControlExpectation {
            name: "dimension-mismatch",
            kind: ExpectedKind::Refused,
            refusal_code: Some("DimensionMismatch"),
            uncertainty: Some(UncertaintyExpectation::NotEstimated),
        },
        ControlExpectation {
            name: "unknown-symbol",
            kind: ExpectedKind::Refused,
            refusal_code: Some("UnknownUnit"),
            uncertainty: Some(UncertaintyExpectation::NotEstimated),
        },
    ],
    coverage: &[&["lbf"], &["n*m", "newton"]],
};

const UNKNOWN_COST: OracleExpectation = OracleExpectation {
    kind: ExpectedKind::Unavailable,
    refusal_codes: NO_ABSENT,
    windows: NO_WINDOWS,
    money: NO_MONEY,
    absent: &["incumbent landed total", "cheap landed total"],
    authority: AuthorityExpectation::AnswerCoverage,
    missing: &[&["classification"], &["tariff"], &["valuation"]],
    boundaries: &[&["unknown"], &["not applicable", "not-applicable"]],
    uncertainty: UncertaintyExpectation::NotEstimated,
    trap: TrapExpectation {
        requires_non_solved: true,
        ..TrapExpectation::DEFAULT
    },
    experiments: &[
        &["duty"],
        &["classification", "tariff"],
        &["zero", "decision"],
    ],
    gates: NO_GATES,
    controls: &[ControlExpectation {
        name: "not-applicable",
        kind: ExpectedKind::Solved,
        refusal_code: None,
        uncertainty: Some(UncertaintyExpectation::Exact),
    }],
    coverage: &[&["duty"]],
};

pub const ID_BY_EXPECTATION: &[(&str, &OracleExpectation)] = &[
    (CASE_IDS[0], &OPAMP),
    (CASE_IDS[1], &LARGE_SIGNAL),
    (CASE_IDS[2], &FLYBACK),
    (CASE_IDS[3], &PCB),
    (CASE_IDS[4], &ALUMINIUM),
    (CASE_IDS[5], &FASTENER),
    (CASE_IDS[6], &FATIGUE),
    (CASE_IDS[7], &THERMAL),
    (CASE_IDS[8], &MOLDING),
    (CASE_IDS[9], &FEA),
    (CASE_IDS[10], &SUPPLIER),
    (CASE_IDS[11], &HS),
    (CASE_IDS[12], &UNITS),
    (CASE_IDS[13], &UNKNOWN_COST),
    (CASE_IDS[14], &EMC),
];

/// Expectation for a case id.
pub fn expectation(id: &str) -> Option<&'static OracleExpectation> {
    ID_BY_EXPECTATION
        .iter()
        .find(|(candidate, _)| *candidate == id)
        .map(|(_, expectation)| *expectation)
}
