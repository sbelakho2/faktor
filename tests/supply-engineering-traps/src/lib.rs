// TEST-SEPARATION: oracle=independent-reference isolation=separate-process
//! Vicious trap benchmark for Faktor Supply + Engineering.
//!
//! The benchmark is split into an untrusted SUT and an independent oracle:
//!
//! * [`sut`] submits fixture requests to the real engineering pipeline
//!   (requirement validation -> authoritative model selection -> unit-aware
//!   kernels -> 11-gate response) or to the real supply cost/logistics/unit
//!   kernels, and returns an opaque [`SutAnswer`].
//! * [`oracle`] owns its own expected numerical windows, refusal codes, gate
//!   states and assumption coverage, re-reads the bundled knowledge registry
//!   for authority, and computes the eight review dimensions.
//!
//! There is no metric boolean, acceptance flag or `with_*` setter in the SUT
//! contract. A case passes only when the independent oracle says so.

use serde::Serialize;
use thiserror::Error;

pub mod contract;
pub mod fixtures;
mod oracle;
mod sut;

pub use contract::{
    ControlAnswer, ExpectedKind, GateRecord, GateStateKind, Observation, SutAnswer, SutOutcome,
    SutUncertainty, SUITE_SCHEMA, SUITE_VERSION,
};
pub use oracle::{
    evaluate as evaluate_answer, expectation as oracle_expectation, AuthorityExpectation,
    ControlExpectation, MoneyWindow, OracleExpectation, OracleVerdict, QuantityWindow,
    TrapExpectation, UncertaintyExpectation, CASE_COUNT, CASE_IDS,
};

pub const MAX_CASES: usize = 32;
pub const MAX_EVIDENCE: usize = 64;
pub const MAX_MISSING_INFO: usize = 64;
pub const MAX_BOUNDARIES: usize = 64;
pub const MAX_OBSERVATIONS: usize = 128;
pub const MAX_TEXT: usize = 4_096;

/// A refused benchmark operation.
#[derive(Debug, Error)]
pub enum BenchError {
    /// The case id is empty.
    #[error("case id must not be empty")]
    EmptyCaseId,
    /// The case id is not part of this suite.
    #[error("unknown trap case {0:?}")]
    UnknownCase(String),
    /// A caller asked for more cases than the fixed bound.
    #[error("case request exceeds the bound of {MAX_CASES}")]
    CaseBound,
    /// A case could not execute its real model/fixture.
    #[error("case {id} failed: {detail}")]
    Case {
        /// Case id.
        id: String,
        /// Deterministic detail.
        detail: String,
    },
    /// A report field exceeded its bound.
    #[error("report field {0} exceeds its bound")]
    ReportBound(&'static str),
    /// JSON serialization failed.
    #[error("report serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Static presentation metadata. It contains no expected result and no
/// acceptance state.
#[derive(Debug, Clone, Copy)]
struct CaseMeta {
    id: &'static str,
    domain: &'static str,
    title: &'static str,
    shallow_claim: &'static str,
}

const CASE_META: &[CaseMeta] = &[
    CaseMeta {
        id: "analog/opamp-nominal-gain-stability",
        domain: "analog",
        title: "Nominal gain is not a stability proof",
        shallow_claim: "The closed-loop gain is 10, therefore the amplifier is stable.",
    },
    CaseMeta {
        id: "analog/opamp-large-signal-slew",
        domain: "analog",
        title: "Small-signal stability does not cover large-signal slew limiting",
        shallow_claim: "The small-signal loop is compensated, so the amplifier is stable for every large-signal step.",
    },
    CaseMeta {
        id: "power/flyback-leakage-overvoltage",
        domain: "power-electronics",
        title: "Flyback leakage spike is outside the conduction-only model",
        shallow_claim: "Vds_peak = Vin + N*Vout is below the MOSFET rating, so the switch is safe.",
    },
    CaseMeta {
        id: "pcb/alternate-thermal-pad",
        domain: "pcb/thermal",
        title: "Alternate PCB with a different thermal pad is not equivalent",
        shallow_claim: "Same copper area and same MOSFET means the alternate PCB is thermally equivalent.",
    },
    CaseMeta {
        id: "materials/aluminium-temper-thickness",
        domain: "materials",
        title: "Wrong temper and thickness invalidate an aluminium strength claim",
        shallow_claim: "Aluminium 6061 has a familiar yield strength, so any 6061 part can use it.",
    },
    CaseMeta {
        id: "machine/fastener-nut-factor-scatter",
        domain: "machine-design",
        title: "Nominal torque hides nut-factor scatter and validity",
        shallow_claim: "K = 0.2 gives 41.7 kN, so the joint preload is known.",
    },
    CaseMeta {
        id: "fatigue/sn-outside-regime",
        domain: "fatigue",
        title: "S-N extrapolation outside the tested regime is not valid",
        shallow_claim: "Endurance limit is half the tensile strength, so the part has infinite life.",
    },
    CaseMeta {
        id: "thermal/missing-contact-resistance",
        domain: "thermal",
        title: "Ignoring contact resistance understates junction temperature",
        shallow_claim: "Use Rth_jc + Rth_sa; the thermal interface is negligible.",
    },
    CaseMeta {
        id: "manufacturing/molding-draft-wall",
        domain: "manufacturing",
        title: "Draft and wall violations make the molded geometry unmanufacturable",
        shallow_claim: "The CAD model closes, so the molded part can be produced.",
    },
    CaseMeta {
        id: "fea/mesh-convergence-pathology",
        domain: "fea",
        title: "A mesh is not a converged finite-element result",
        shallow_claim: "The solver exited successfully and the mesh has many elements, so the stress is converged.",
    },
    CaseMeta {
        id: "supply/cheaper-supplier-tco",
        domain: "supply/tco",
        title: "Cheaper unit price vanishes under landed TCO",
        shallow_claim: "$8.50 is cheaper than $10.00, so the cheap supplier saves money.",
    },
    CaseMeta {
        id: "customs/hs-classification-ambiguity",
        domain: "customs",
        title: "Classification ambiguity must escalate, not pick a code",
        shallow_claim: "Choose the candidate with the lower duty rate and declare it.",
    },
    CaseMeta {
        id: "units/torque-lbf-ft",
        domain: "units",
        title: "lbf*ft and N*m are not interchangeable numbers",
        shallow_claim: "100 ft-lb equals 100 N-m; the number is the same.",
    },
    CaseMeta {
        id: "supply/unknown-cost-not-zero",
        domain: "supply/tco",
        title: "An unknown cost term is unavailable, never zero",
        shallow_claim: "Duty is unknown but probably small, so set it to zero and compare totals.",
    },
    CaseMeta {
        id: "emc/return-path-loop-inductance",
        domain: "emc/pcb",
        title: "Equal copper area does not preserve the return-path loop",
        shallow_claim: "The layouts use the same copper area, so their EMC behaviour is equivalent.",
    },
];

/// One trap case result. The SUT answer is opaque; the oracle verdict is the
/// only acceptance state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CaseReport {
    /// Stable case id.
    pub id: String,
    /// Domain pack / discipline.
    pub domain: String,
    /// Short title.
    pub title: String,
    /// What a shallow answer would claim.
    pub shallow_claim: String,
    /// The expected outcome class (owned by the oracle).
    pub expected: ExpectedKind,
    /// The opaque SUT result.
    pub sut: SutAnswer,
    /// The independent oracle verdict.
    pub oracle: OracleVerdict,
}

impl CaseReport {
    /// True only when the independent oracle passes all eight dimensions.
    pub fn passed(&self) -> bool {
        self.oracle.all()
    }
}

/// Per-metric pass counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MetricPass {
    /// Cases passing correct physics.
    pub correct_physics: usize,
    /// Cases passing authority.
    pub authority: usize,
    /// Cases passing missing-information detection.
    pub missing_info_detected: usize,
    /// Cases passing boundary conditions.
    pub boundary_conditions: usize,
    /// Cases passing uncertainty.
    pub uncertainty: usize,
    /// Cases passing trap detection.
    pub trap_detected: usize,
    /// Cases passing falsifiable experiment construction.
    pub falsifiable_experiment: usize,
    /// Cases passing refusal/rejection where appropriate.
    pub refused_where_appropriate: usize,
}

/// Suite summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Total cases.
    pub total: usize,
    /// Cases whose oracle verdict passes.
    pub passed: usize,
    /// Cases that failed the oracle.
    pub failed: usize,
    /// SUT outcomes by class.
    pub solved: usize,
    /// SUT rejected designs.
    pub rejected: usize,
    /// SUT typed refusals.
    pub refused: usize,
    /// SUT research-required outcomes.
    pub research_required: usize,
    /// SUT unavailable outcomes.
    pub unavailable: usize,
    /// SUT ambiguity escalations.
    pub ambiguous: usize,
    /// Per-metric pass counts.
    pub metric_pass: MetricPass,
    /// Failed case ids.
    pub failed_cases: Vec<String>,
}

impl Summary {
    /// True only when every case passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0 && self.passed == self.total && self.total > 0
    }
}

/// The full machine-readable suite report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SuiteReport {
    /// Report schema.
    pub schema: String,
    /// Suite version.
    pub version: String,
    /// Case results in deterministic dispatch order.
    pub cases: Vec<CaseReport>,
    /// Aggregate summary.
    pub summary: Summary,
}

impl SuiteReport {
    /// Build a report from cases, enforcing the suite bound.
    pub fn from_cases(cases: Vec<CaseReport>) -> Result<Self, BenchError> {
        if cases.is_empty() || cases.len() > MAX_CASES {
            return Err(BenchError::CaseBound);
        }
        for case in &cases {
            validate_report(case)?;
        }
        let summary = summarize(&cases);
        Ok(Self {
            schema: SUITE_SCHEMA.to_string(),
            version: SUITE_VERSION.to_string(),
            cases,
            summary,
        })
    }

    /// JSON bytes (pretty or compact) with deterministic field order.
    pub fn to_json(&self, pretty: bool) -> Result<String, BenchError> {
        if pretty {
            Ok(serde_json::to_string_pretty(self)?)
        } else {
            Ok(serde_json::to_string(self)?)
        }
    }
}

fn validate_report(report: &CaseReport) -> Result<(), BenchError> {
    if report.id.trim().is_empty() || report.id.len() > MAX_TEXT {
        return Err(BenchError::EmptyCaseId);
    }
    let sut = &report.sut;
    if sut.evidence.len() > MAX_EVIDENCE {
        return Err(BenchError::ReportBound("evidence"));
    }
    if sut.missing_info.len() > MAX_MISSING_INFO {
        return Err(BenchError::ReportBound("missing_info"));
    }
    if sut.boundary_conditions.len() > MAX_BOUNDARIES {
        return Err(BenchError::ReportBound("boundary_conditions"));
    }
    if sut.outcome.observations().len() > MAX_OBSERVATIONS {
        return Err(BenchError::ReportBound("observations"));
    }
    if sut.experiments.is_empty()
        || sut.experiments.iter().any(|test| {
            test.claim.trim().is_empty()
                || test.procedure.trim().is_empty()
                || test.pass_criterion.trim().is_empty()
                || test.refutation.trim().is_empty()
                || test.instrument.trim().is_empty()
        })
    {
        return Err(BenchError::ReportBound("falsifiable_experiment"));
    }
    if report.shallow_claim.trim().is_empty() {
        return Err(BenchError::ReportBound("shallow_claim"));
    }
    Ok(())
}

fn summarize(cases: &[CaseReport]) -> Summary {
    let mut summary = Summary {
        total: cases.len(),
        passed: 0,
        failed: 0,
        solved: 0,
        rejected: 0,
        refused: 0,
        research_required: 0,
        unavailable: 0,
        ambiguous: 0,
        metric_pass: MetricPass::default(),
        failed_cases: Vec::new(),
    };
    for case in cases {
        match case.sut.outcome.kind() {
            ExpectedKind::Solved => summary.solved += 1,
            ExpectedKind::Rejected => summary.rejected += 1,
            ExpectedKind::Refused => summary.refused += 1,
            ExpectedKind::ResearchRequired => summary.research_required += 1,
            ExpectedKind::Unavailable => summary.unavailable += 1,
            ExpectedKind::Ambiguous => summary.ambiguous += 1,
        }
        let oracle = &case.oracle;
        if oracle.correct_physics {
            summary.metric_pass.correct_physics += 1;
        }
        if oracle.authority {
            summary.metric_pass.authority += 1;
        }
        if oracle.missing_info_detected {
            summary.metric_pass.missing_info_detected += 1;
        }
        if oracle.boundary_conditions {
            summary.metric_pass.boundary_conditions += 1;
        }
        if oracle.uncertainty {
            summary.metric_pass.uncertainty += 1;
        }
        if oracle.trap_detected {
            summary.metric_pass.trap_detected += 1;
        }
        if oracle.falsifiable_experiment {
            summary.metric_pass.falsifiable_experiment += 1;
        }
        if oracle.refused_where_appropriate {
            summary.metric_pass.refused_where_appropriate += 1;
        }
        if case.passed() {
            summary.passed += 1;
        } else {
            summary.failed += 1;
            summary.failed_cases.push(case.id.clone());
        }
    }
    summary
}

/// Run every case in the fixed suite order.
pub fn run_suite() -> Result<SuiteReport, BenchError> {
    let cases = case_ids()
        .iter()
        .map(|id| run_case(id))
        .collect::<Result<Vec<_>, _>>()?;
    SuiteReport::from_cases(cases)
}

/// Run one case by id. The SUT runs first; the oracle is evaluated against
/// the resulting opaque answer and its own expectation.
pub fn run_case(id: &str) -> Result<CaseReport, BenchError> {
    if id.trim().is_empty() {
        return Err(BenchError::EmptyCaseId);
    }
    let meta = CASE_META
        .iter()
        .find(|meta| meta.id == id)
        .ok_or_else(|| BenchError::UnknownCase(id.to_string()))?;
    let expectation =
        oracle_expectation(id).ok_or_else(|| BenchError::UnknownCase(id.to_string()))?;
    let sut = sut::run(id)?;
    let oracle = evaluate_answer(expectation, &sut);
    Ok(CaseReport {
        id: meta.id.to_string(),
        domain: meta.domain.to_string(),
        title: meta.title.to_string(),
        shallow_claim: meta.shallow_claim.to_string(),
        expected: expectation.kind,
        sut,
        oracle,
    })
}

/// The stable case ids in dispatch order.
pub fn case_ids() -> Vec<&'static str> {
    CASE_IDS.to_vec()
}

/// A deliberately wrong SUT answer for negative-control tests. It preserves
/// the case id and experiment but replaces the outcome with the shallow claim:
/// a solved answer with no authoritative observations.
pub fn negative_control(case: &CaseReport) -> SutAnswer {
    let observations = if case.sut.outcome.kind() == ExpectedKind::Solved {
        case.sut
            .outcome
            .observations()
            .iter()
            .map(|observation| match observation {
                Observation::Money {
                    name,
                    currency,
                    micros,
                } => Observation::Money {
                    name: name.clone(),
                    micros: micros.saturating_sub(1),
                    currency: currency.clone(),
                },
                Observation::Quantity { name, unit, .. } => Observation::Quantity {
                    name: name.clone(),
                    value: 0.0,
                    unit: unit.clone(),
                },
                Observation::Text { name, value } => Observation::Text {
                    name: name.clone(),
                    value: value.clone(),
                },
            })
            .collect()
    } else {
        Vec::new()
    };
    SutAnswer {
        id: case.sut.id.clone(),
        outcome: SutOutcome::Solved { observations },
        gate_states: Vec::new(),
        missing_info: Vec::new(),
        boundary_conditions: Vec::new(),
        uncertainty: SutUncertainty::Exact,
        assumption_coverage: Vec::new(),
        evidence: Vec::new(),
        experiments: case.sut.experiments.clone(),
        controls: Vec::new(),
    }
}
