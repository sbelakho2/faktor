//! Engineering trap fixtures submitted to the real pipeline.

use std::collections::BTreeMap;

use engineering_core::{
    AssumptionRecord, AuthorityRef, CounterexampleProbe, EngineeringRequirement,
    EngineeringRequirementSet, FalsifiableTest, GateId, KernelProbe, KernelReport, ModelBinding,
    PipelineBlocker, PipelineRequest, ProbeOutcome, Provenance, ProvenanceKind,
    RealizationCategory, RealizationConsequence, ResearchRequest, RevisionId, SelectionRequest,
    ValueState,
};
use engineering_knowledge::{KnowledgePack, KnowledgeRegistry};
use engineering_solvers::{AnalysisRequest, EngineeringSolver, GmshSolver};
use engineering_units::{Quantity, UnitRegistry};

use crate::contract::{ControlAnswer, SutAnswer};
use crate::sut::kernel;
use crate::sut::{answer_from_run, control_from_report, report_uncertainty};
use crate::BenchError;

pub(crate) const OPAMP_ID: &str = "analog/opamp-nominal-gain-stability";
pub(crate) const LARGE_SIGNAL_ID: &str = "analog/opamp-large-signal-slew";
pub(crate) const FLYBACK_ID: &str = "power/flyback-leakage-overvoltage";
pub(crate) const PCB_ID: &str = "pcb/alternate-thermal-pad";
pub(crate) const ALUMINIUM_ID: &str = "materials/aluminium-temper-thickness";
pub(crate) const FASTENER_ID: &str = "machine/fastener-nut-factor-scatter";
pub(crate) const FATIGUE_ID: &str = "fatigue/sn-outside-regime";
pub(crate) const THERMAL_ID: &str = "thermal/missing-contact-resistance";
pub(crate) const MOLDING_ID: &str = "manufacturing/molding-draft-wall";
pub(crate) const FEA_ID: &str = "fea/mesh-convergence-pathology";
pub(crate) const EMC_ID: &str = "emc/return-path-loop-inductance";

const MOSFET_MODEL: &str = "model-conduction-thermal";
const BOLT_MODEL: &str = "model-torque-preload";
const MOLDING_MODEL: &str = "model-moldability-checks";

fn case_error(id: &str, detail: impl std::fmt::Display) -> BenchError {
    BenchError::Case {
        id: id.to_string(),
        detail: detail.to_string(),
    }
}

fn registry() -> Result<KnowledgeRegistry, BenchError> {
    KnowledgeRegistry::bundled().map_err(|error| case_error("knowledge", error))
}

fn fixture_provenance(citation: &str) -> Provenance {
    Provenance::new(ProvenanceKind::Assumption, citation).expect("static fixture citation")
}

fn authority_of(
    registry: &KnowledgeRegistry,
    pack_ids: &[&str],
) -> Result<Vec<AuthorityRef>, BenchError> {
    pack_ids
        .iter()
        .map(|pack_id| {
            registry
                .latest(pack_id)
                .and_then(KnowledgePack::authority)
                .ok_or_else(|| case_error(pack_id, "bundled pack or authority digest is missing"))
        })
        .collect()
}

fn model_binding(
    registry: &KnowledgeRegistry,
    pack_id: &str,
    model_id: &str,
) -> Result<ModelBinding, BenchError> {
    let pack = registry
        .latest(pack_id)
        .ok_or_else(|| case_error(pack_id, "bundled pack is missing"))?;
    let model = pack
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or_else(|| case_error(model_id, "pack model is missing"))?;
    Ok(ModelBinding {
        model_id: model.id.clone(),
        version: pack.version.clone(),
        equation_ids: model.equation_ids.clone(),
        temperature: None,
        conditions: Vec::new(),
        exclusions: Vec::new(),
        pack_id: Some(pack.id.clone()),
        pack_version: Some(pack.version.clone()),
        provenance: fixture_provenance("benchmark fixture: model binding to the bundled pack"),
    })
}

fn numeric_requirement(
    id: &str,
    statement: &str,
    discipline: engineering_core::Discipline,
    property: &str,
    limit: engineering_core::NumericLimit,
    model: Option<&str>,
) -> EngineeringRequirement {
    EngineeringRequirement {
        id: id.to_string(),
        statement: statement.to_string(),
        discipline,
        mandatory: true,
        target: engineering_core::RequirementTarget::Numeric {
            property: property.to_string(),
            limit,
            model: model.map(str::to_string),
        },
        conditions: Vec::new(),
        provenance: fixture_provenance("benchmark fixture: hypothetical acceptance requirement"),
    }
}

fn categorical_requirement(
    id: &str,
    statement: &str,
    discipline: engineering_core::Discipline,
    property: &str,
    allowed: &[&str],
) -> EngineeringRequirement {
    EngineeringRequirement {
        id: id.to_string(),
        statement: statement.to_string(),
        discipline,
        mandatory: true,
        target: engineering_core::RequirementTarget::Categorical {
            property: property.to_string(),
            allowed: allowed.iter().map(|value| (*value).to_string()).collect(),
            model: None,
        },
        conditions: Vec::new(),
        provenance: fixture_provenance("benchmark fixture: hypothetical acceptance requirement"),
    }
}

fn requirement_set(
    id: &str,
    objective: &str,
    requirements: Vec<EngineeringRequirement>,
    models: Vec<ModelBinding>,
) -> EngineeringRequirementSet {
    EngineeringRequirementSet {
        id: id.to_string(),
        revision: RevisionId::new(1),
        objective: objective.to_string(),
        scope: "benchmark fixture: synthetic inputs, no external evidence".to_string(),
        requirements,
        constraints: Vec::new(),
        models,
        evidence: Vec::new(),
    }
}

fn selection(
    requirement_id: &str,
    discipline: engineering_core::Discipline,
    property: &str,
) -> SelectionRequest {
    SelectionRequest {
        requirement_id: requirement_id.to_string(),
        discipline,
        property: Some(property.to_string()),
        conditions: Vec::new(),
        material: None,
        temperature: None,
    }
}

fn falsifiable(
    claim: &str,
    procedure: &str,
    pass_criterion: &str,
    refutation: &str,
    instrument: &str,
    evidence: &[&str],
) -> FalsifiableTest {
    FalsifiableTest {
        claim: claim.to_string(),
        procedure: procedure.to_string(),
        pass_criterion: pass_criterion.to_string(),
        refutation: refutation.to_string(),
        instrument: instrument.to_string(),
        evidence: evidence.iter().map(|item| (*item).to_string()).collect(),
    }
}

fn assumption(id: &str, statement: &str) -> AssumptionRecord {
    AssumptionRecord {
        id: id.to_string(),
        statement: statement.to_string(),
        provenance: fixture_provenance("benchmark fixture: explicit assumption, not evidence"),
    }
}

fn counterexample(probe: &str, regime: &str) -> CounterexampleProbe {
    CounterexampleProbe {
        probe: probe.to_string(),
        regime: regime.to_string(),
        outcome: ProbeOutcome::NoFailureFound,
        evidence: vec!["control executed against the same kernel".to_string()],
    }
}

fn realization(category: RealizationCategory, statement: &str) -> RealizationConsequence {
    RealizationConsequence {
        category,
        statement: statement.to_string(),
        availability: ValueState::known("benchmark fixture: no production evidence".to_string()),
        evidence: vec!["benchmark fixture".to_string()],
    }
}

fn base_request(
    id: &str,
    requirements: EngineeringRequirementSet,
    selections: Vec<SelectionRequest>,
    authority: Vec<AuthorityRef>,
) -> PipelineRequest {
    PipelineRequest {
        id: id.to_string(),
        requirements,
        catalog: registry()
            .expect("bundled registry")
            .model_catalog()
            .expect("bundled catalog"),
        selections,
        probes: Vec::new(),
        authority,
        assumptions: Vec::new(),
        unknowns: Vec::new(),
        tests: Vec::new(),
        counterexamples: Vec::new(),
        realization: Vec::new(),
        boundary_conditions: Vec::new(),
        blocker: None,
    }
}

fn execute(probe: &KernelProbe) -> engineering_core::CoreResult<KernelReport> {
    let result = match probe.model_id.as_deref() {
        Some(MOSFET_MODEL) => kernel::thermal(probe),
        Some(BOLT_MODEL) => kernel::fastener(probe),
        Some(MOLDING_MODEL) => kernel::molding(probe),
        Some("model-opamp-loop-stability") => kernel::analog(probe),
        Some("model-pcb-loop-inductance") => kernel::emc(probe),
        other => Err(case_error(
            &probe.id,
            format!("no kernel is registered for model {other:?}"),
        )),
    };
    result.map_err(|error| engineering_core::CoreError::InvalidValue {
        reason: error.to_string(),
    })
}

fn build(
    id: &str,
    request: PipelineRequest,
    mut coverage: Vec<String>,
    controls: Vec<ControlAnswer>,
) -> Result<SutAnswer, BenchError> {
    let authority = request.authority.clone();
    let experiments = request.tests.clone();
    let run = engineering_core::run_engineering_pipeline(request, execute)
        .map_err(|error| case_error(id, error))?;
    let mut answer = answer_from_run(id, &run, &authority, experiments, controls);
    answer.assumption_coverage.append(&mut coverage);
    Ok(answer)
}

fn q(value: f64, unit: &str) -> Result<Quantity, BenchError> {
    Quantity::new(
        value,
        UnitRegistry::si()
            .require(unit)
            .map_err(|error| case_error("fixture", error))?,
    )
    .map_err(|error| case_error("fixture", error))
}

fn thermal_probe(id: &str, with_contact: bool) -> Result<KernelProbe, BenchError> {
    let mut inputs = BTreeMap::new();
    inputs.insert("drain_current_rms".to_string(), q(20.0, "A")?);
    inputs.insert("rds_on_reference".to_string(), q(10.0, "mohm")?);
    inputs.insert("temp_coefficient".to_string(), q(0.004, "1/K")?);
    inputs.insert("reference_temperature".to_string(), q(25.0, "degC")?);
    inputs.insert("ambient_temperature".to_string(), q(40.0, "degC")?);
    inputs.insert("r_theta_jc".to_string(), q(1.5, "K/W")?);
    inputs.insert("r_theta_sa".to_string(), q(2.0, "K/W")?);
    inputs.insert("max_junction_temperature".to_string(), q(150.0, "degC")?);
    if with_contact {
        inputs.insert("r_theta_cs".to_string(), q(0.40, "K/W")?);
    }
    Ok(KernelProbe {
        id: id.to_string(),
        model_id: Some(MOSFET_MODEL.to_string()),
        inputs,
        labels: BTreeMap::new(),
    })
}

fn zero_interface_probe(id: &str) -> Result<KernelProbe, BenchError> {
    let mut probe = thermal_probe(id, true)?;
    probe
        .inputs
        .insert("r_theta_cs".to_string(), q(0.0, "K/W")?);
    Ok(probe)
}

fn fastener_probe(id: &str, k_lower: f64, k_upper: f64) -> Result<KernelProbe, BenchError> {
    let mut inputs = BTreeMap::new();
    inputs.insert("torque".to_string(), q(100.0, "N*m")?);
    inputs.insert("nominal_diameter".to_string(), q(12.0, "mm")?);
    inputs.insert("thread_pitch".to_string(), q(1.75, "mm")?);
    inputs.insert("nut_factor".to_string(), q(0.2, "1")?);
    inputs.insert("nut_factor_lower".to_string(), q(k_lower, "1")?);
    inputs.insert("nut_factor_upper".to_string(), q(k_upper, "1")?);
    inputs.insert("validity_lower".to_string(), q(0.12, "1")?);
    inputs.insert("validity_upper".to_string(), q(0.28, "1")?);
    inputs.insert("proof_strength".to_string(), q(640.0, "MPa")?);
    Ok(KernelProbe {
        id: id.to_string(),
        model_id: Some(BOLT_MODEL.to_string()),
        inputs,
        labels: BTreeMap::new(),
    })
}

fn molding_probe(
    id: &str,
    draft: f64,
    draft_unit: &str,
    nominal: f64,
    min: f64,
    max: f64,
    flow: f64,
) -> Result<KernelProbe, BenchError> {
    let mut inputs = BTreeMap::new();
    inputs.insert("nominal_wall".to_string(), q(nominal, "mm")?);
    inputs.insert("min_wall".to_string(), q(min, "mm")?);
    inputs.insert("max_wall".to_string(), q(max, "mm")?);
    inputs.insert("draft_actual".to_string(), q(draft, draft_unit)?);
    inputs.insert("flow_length".to_string(), q(flow, "mm")?);
    Ok(KernelProbe {
        id: id.to_string(),
        model_id: Some(MOLDING_MODEL.to_string()),
        inputs,
        labels: BTreeMap::new(),
    })
}

fn analog_probe(id: &str, noise_gain: f64) -> Result<KernelProbe, BenchError> {
    let mut inputs = BTreeMap::new();
    inputs.insert("open_loop_gain".to_string(), q(100_000.0, "1")?);
    inputs.insert("first_pole".to_string(), q(10.0, "Hz")?);
    inputs.insert("second_pole".to_string(), q(1_000_000.0, "Hz")?);
    inputs.insert("output_resistance".to_string(), q(100.0, "ohm")?);
    inputs.insert("load_capacitance".to_string(), q(1.0, "uF")?);
    inputs.insert("opamp_output_capacitance".to_string(), q(10.0, "pF")?);
    inputs.insert("input_capacitance".to_string(), q(1.0, "nF")?);
    inputs.insert("feedback_resistance".to_string(), q(10_000.0, "ohm")?);
    inputs.insert("noise_gain".to_string(), q(noise_gain, "1")?);
    inputs.insert("required_phase_margin_deg".to_string(), q(45.0, "deg")?);
    inputs.insert("required_gain_margin_db".to_string(), q(10.0, "dB")?);
    Ok(KernelProbe {
        id: id.to_string(),
        model_id: Some("model-opamp-loop-stability".to_string()),
        inputs,
        labels: BTreeMap::new(),
    })
}

fn emc_probe(
    id: &str,
    distance: f64,
    clearance: f64,
    creepage: f64,
) -> Result<KernelProbe, BenchError> {
    let mut inputs = BTreeMap::new();
    inputs.insert("stackup_height".to_string(), q(0.2, "mm")?);
    inputs.insert("copper_thickness".to_string(), q(35.0, "um")?);
    inputs.insert("trace_width".to_string(), q(0.3, "mm")?);
    inputs.insert("capacitor_distance".to_string(), q(distance, "mm")?);
    inputs.insert("via_length".to_string(), q(1.0, "mm")?);
    inputs.insert("frequency".to_string(), q(100.0, "MHz")?);
    inputs.insert("max_loop_inductance".to_string(), q(10.0, "nH")?);
    inputs.insert("actual_clearance".to_string(), q(clearance, "mm")?);
    inputs.insert("actual_creepage".to_string(), q(creepage, "mm")?);
    inputs.insert("min_clearance".to_string(), q(0.6, "mm")?);
    inputs.insert("min_creepage".to_string(), q(0.6, "mm")?);
    Ok(KernelProbe {
        id: id.to_string(),
        model_id: Some("model-pcb-loop-inductance".to_string()),
        inputs,
        labels: BTreeMap::new(),
    })
}

fn emc_controls() -> Result<Vec<ControlAnswer>, BenchError> {
    let adjacent = kernel::emc(&emc_probe("control-adjacent-placement", 0.0, 0.7, 0.7)?)?;
    Ok(vec![control_from_report(
        "adjacent-placement",
        &adjacent,
        report_uncertainty(&adjacent),
    )])
}

fn analog_controls() -> Result<Vec<ControlAnswer>, BenchError> {
    let compensated = kernel::analog(&analog_probe("control-compensated", 600.0)?)?;
    let mut unordered = analog_probe("control-unordered-poles", 10.0)?;
    unordered
        .inputs
        .insert("second_pole".to_string(), q(5.0, "Hz")?);
    let refused = kernel::analog(&unordered)?;
    Ok(vec![
        control_from_report(
            "compensated",
            &compensated,
            report_uncertainty(&compensated),
        ),
        control_from_report("unordered-poles", &refused, report_uncertainty(&refused)),
    ])
}

fn thermal_controls() -> Result<Vec<ControlAnswer>, BenchError> {
    let with_contact = kernel::thermal(&thermal_probe("control-with-contact", true)?)?;
    let zero = kernel::thermal(&zero_interface_probe("control-zero-interface")?)?;
    Ok(vec![
        control_from_report(
            "with-contact",
            &with_contact,
            report_uncertainty(&with_contact),
        ),
        control_from_report("zero-interface", &zero, report_uncertainty(&zero)),
    ])
}

fn fastener_controls() -> Result<Vec<ControlAnswer>, BenchError> {
    let valid = kernel::fastener(&fastener_probe("control-valid-scatter", 0.18, 0.22)?)?;
    let invalid = kernel::fastener(&fastener_probe("control-invalid-scatter", 0.05, 0.50)?)?;
    Ok(vec![
        control_from_report("valid-scatter", &valid, report_uncertainty(&valid)),
        control_from_report("invalid-scatter", &invalid, report_uncertainty(&invalid)),
    ])
}

fn molding_controls() -> Result<Vec<ControlAnswer>, BenchError> {
    let radians = kernel::molding(&molding_probe(
        "control-radians",
        0.5,
        "rad",
        2.5,
        2.2,
        2.9,
        400.0,
    )?)?;
    let good = kernel::molding(&molding_probe(
        "control-good",
        1.5,
        "deg",
        2.5,
        2.4,
        2.6,
        300.0,
    )?)?;
    Ok(vec![
        control_from_report("radian-control", &radians, report_uncertainty(&radians)),
        control_from_report("good-control", &good, report_uncertainty(&good)),
    ])
}

fn opamp() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let authority = authority_of(&registry, &["analog-stability"])?;
    let binding = model_binding(&registry, "analog-stability", "model-opamp-loop-stability")?;
    let requirements = requirement_set(
        "opamp-stability",
        "Establish closed-loop phase margin with the actual load.",
        vec![numeric_requirement(
            "req-opamp-phase",
            "Phase margin meets the required stability margin.",
            engineering_core::Discipline::Electrical,
            "phase_margin",
            engineering_core::NumericLimit::Min {
                limit: q(45.0, "deg")?,
            },
            Some("model-opamp-loop-stability"),
        )],
        vec![binding],
    );
    let mut request = base_request(
        OPAMP_ID,
        requirements,
        vec![selection(
            "req-opamp-phase",
            engineering_core::Discipline::Electrical,
            "phase_margin",
        )],
        authority,
    );
    request.probes = vec![analog_probe("opamp-loaded", 10.0)?];
    request.unknowns = vec![
        "device lot variation of the open-loop poles".to_string(),
        "PCB parasitics and load capacitance at temperature".to_string(),
        "large-signal slew and output-saturation limits".to_string(),
    ];
    request.boundary_conditions = vec![
        "phase margin must be at least 45 deg and gain margin at least 10 dB".to_string(),
        "the declared second pole must be above the first pole".to_string(),
        "small-signal linear operation without slew limiting or output saturation".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-nominal-gain-only",
        "The shallow claim supplies only a nominal closed-loop gain of 10.",
    )];
    request.tests = vec![falsifiable(
        "A closed-loop gain of 10 is stable at the specified load and temperature.",
        "Break the loop and measure open-loop gain/phase versus frequency at the worst-case load, output level and temperature, then apply a small input step and record overshoot, ringing and settling.",
        "Phase margin >= 45 deg and gain margin >= 10 dB with no sustained oscillation at any specified corner.",
        "Sustained oscillation, phase margin < 45 deg, gain margin < 10 dB, or load-dependent instability.",
        "Vector network analyzer or loop-gain injection probe plus a high-bandwidth oscilloscope.",
        &[
            "the case supplies only a nominal gain of 10",
            "the actual 1 uF load moves the output pole and destroys the phase margin",
            "the model cross-checks the loop-gain calculation independently",
        ],
    )];
    request.counterexamples = vec![
        counterexample("nominal gain only", "loaded loop"),
        counterexample("noise gain 600", "compensation control"),
    ];
    request.realization = vec![realization(
        RealizationCategory::Test,
        "Stability must be measured on the actual load; nominal gain cannot be tested into existence.",
    )];
    build(
        OPAMP_ID,
        request,
        vec![
            "prop-opamp-loop".to_string(),
            "rule-phase-margin".to_string(),
            "trap-nominal-gain-stability".to_string(),
            "model-opamp-loop-stability".to_string(),
        ],
        analog_controls()?,
    )
}

fn large_signal() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let pack = registry
        .latest("analog-stability")
        .ok_or_else(|| case_error(LARGE_SIGNAL_ID, "analog-stability pack is missing"))?;
    let text = serde_json::to_string(pack)
        .unwrap_or_default()
        .to_lowercase();
    if !text.contains("large-signal slew-rate limiting") || !text.contains("slew-rate") {
        return Err(case_error(
            LARGE_SIGNAL_ID,
            "the analog pack no longer excludes large-signal slew limiting; research-required is stale",
        ));
    }
    let authority = authority_of(&registry, &["analog-stability"])?;
    let requirements = requirement_set(
        "opamp-large-signal",
        "Establish large-signal stability under slew limiting.",
        vec![categorical_requirement(
            "req-opamp-large-signal",
            "The amplifier remains stable under the specified large-signal step.",
            engineering_core::Discipline::Electrical,
            "large_signal_stability",
            &["stable"],
        )],
        Vec::new(),
    );
    let mut request = base_request(
        LARGE_SIGNAL_ID,
        requirements,
        vec![selection(
            "req-opamp-large-signal",
            engineering_core::Discipline::Electrical,
            "large_signal_stability",
        )],
        authority,
    );
    request.selections[0].conditions = vec!["large-signal slew-rate limiting".to_string()];
    let missing = vec![
        "large-signal slew-rate and output-saturation data under the actual load".to_string(),
        "slew-rate limiting and overload recovery measurements".to_string(),
        "thermal drift and device lot variation at the large-signal operating point".to_string(),
        "a validated large-signal/overdrive stability model".to_string(),
    ];
    request.unknowns = missing.clone();
    request.boundary_conditions = vec![
        "the model applies only to small-signal linear operation without slew limiting"
            .to_string(),
        "large-signal slew-rate limiting and output saturation are outside the bundled validity envelope"
            .to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-nominal-gain-only",
        "The shallow claim supplies only a nominal closed-loop gain of 10.",
    )];
    request.tests = vec![falsifiable(
        "The closed-loop gain of 10 remains stable under the specified large-signal step.",
        "Drive the actual loaded amplifier with the worst-case large-signal step and record slew rate, overload recovery, overshoot, ringing and settling; repeat at temperature and device corners.",
        "The output settles without slew-rate-induced oscillation or saturation-induced instability at every specified corner.",
        "Slew limiting, overload recovery or saturation produces sustained oscillation or a settling failure.",
        "High-bandwidth oscilloscope, function/step generator, load fixture, thermal chamber.",
        &[
            "the bundled analog model is small-signal and explicitly excludes large-signal slew-rate limiting",
            "no large-signal/overdrive stability model is registered",
        ],
    )];
    request.counterexamples = vec![counterexample(
        "nominal small-signal gain",
        "large-signal step",
    )];
    request.realization = vec![realization(
        RealizationCategory::Test,
        "Large-signal stability requires step-response testing with a validated model.",
    )];
    request.blocker = Some(PipelineBlocker::Research(ResearchRequest {
        gate: GateId::Validity,
        reason: "the bundled analog authority is small-signal only and explicitly excludes large-signal slew-rate limiting; no large-signal stability model is registered".to_string(),
        sources_required: missing,
    }));
    build(
        LARGE_SIGNAL_ID,
        request,
        vec!["val-linear-small-signal".to_string()],
        Vec::new(),
    )
}

fn flyback() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let pack = registry
        .latest("flyback-converters")
        .ok_or_else(|| case_error(FLYBACK_ID, "flyback-converters pack is missing"))?;
    let text = serde_json::to_string(pack)
        .unwrap_or_default()
        .to_lowercase();
    if !text.contains("avalanche operation") {
        return Err(case_error(
            FLYBACK_ID,
            "the flyback pack no longer excludes avalanche operation; research-required is stale",
        ));
    }
    let authority = authority_of(&registry, &["flyback-converters"])?;
    let requirements = requirement_set(
        "flyback-survival",
        "Establish MOSFET survival through the turn-off leakage spike.",
        vec![categorical_requirement(
            "req-flyback-survival",
            "The MOSFET survives the worst-case turn-off spike without avalanche failure.",
            engineering_core::Discipline::Electrical,
            "mosfet_survival",
            &["safe"],
        )],
        Vec::new(),
    );
    let mut request = base_request(
        FLYBACK_ID,
        requirements,
        vec![selection(
            "req-flyback-survival",
            engineering_core::Discipline::Electrical,
            "vds_peak",
        )],
        authority,
    );
    request.selections[0].conditions = vec!["avalanche operation".to_string()];
    let missing = vec![
        "transformer leakage inductance and coupling coefficient".to_string(),
        "primary current slope at turn-off and the actual switching waveform".to_string(),
        "snubber/clamp topology and its parasitic elements".to_string(),
        "MOSFET Vds rating, avalanche energy rating and thermal impedance at the switching transient".to_string(),
        "worst-case high-line, full-load and maximum-temperature operating point".to_string(),
    ];
    request.unknowns = missing.clone();
    request.boundary_conditions = vec![
        "the bundled flyback authority excludes avalanche operation".to_string(),
        "peak Vds and avalanche energy must be measured at the worst-case corner".to_string(),
        "the reflected voltage alone does not bound the leakage-inductance spike".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-reflected-voltage-only",
        "The shallow claim uses Vin + N*Vout and omits the leakage-inductance spike.",
    )];
    request.tests = vec![falsifiable(
        "The reflected voltage Vin + N*Vout is the complete MOSFET drain-source stress.",
        "Measure Vds with a high-voltage differential probe and primary current with a current shunt at worst-case high line, full load and maximum temperature, including turn-off; record avalanche energy if the clamp is exceeded.",
        "Peak Vds stays below the derated Vds rating with margin, and avalanche energy stays below the rated avalanche energy.",
        "Peak Vds reaches the derated rating, enters avalanche, or the measured leakage spike invalidates the reflected-voltage-only estimate.",
        "High-voltage differential probe, current shunt, isolated high-bandwidth oscilloscope.",
        &[
            "the bundled authority explicitly excludes avalanche operation",
            "no avalanche-energy or leakage-clamp model is registered for this regime",
        ],
    )];
    request.counterexamples = vec![counterexample(
        "reflected-voltage-only estimate",
        "turn-off at high line",
    )];
    request.realization = vec![realization(
        RealizationCategory::Reliability,
        "A reflected-voltage-only design can exceed the Vds rating at turn-off.",
    )];
    request.blocker = Some(PipelineBlocker::Research(ResearchRequest {
        gate: GateId::Validity,
        reason: "the bundled flyback authority explicitly excludes avalanche operation; leakage-energy survival cannot be established".to_string(),
        sources_required: missing,
    }));
    build(
        FLYBACK_ID,
        request,
        vec!["val-clamp-above-reflected".to_string()],
        Vec::new(),
    )
}

fn pcb() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let authority = authority_of(&registry, &["power-electronics"])?;
    let binding = model_binding(&registry, "power-electronics", MOSFET_MODEL)?;
    let requirements = requirement_set(
        "pcb-thermal",
        "Establish alternate-PCB junction temperature with the actual interface.",
        vec![numeric_requirement(
            "req-pcb-tj",
            "Junction temperature stays below the rated maximum.",
            engineering_core::Discipline::Thermal,
            "junction_temperature",
            engineering_core::NumericLimit::Max {
                limit: q(150.0, "degC")?,
            },
            Some(MOSFET_MODEL),
        )],
        vec![binding],
    );
    let mut request = base_request(
        PCB_ID,
        requirements,
        vec![selection(
            "req-pcb-tj",
            engineering_core::Discipline::Thermal,
            "junction_temperature",
        )],
        authority,
    );
    request.probes = vec![thermal_probe("pcb-alternate", false)?];
    request.unknowns = vec![
        "alternate PCB thermal-pad geometry, copper spreading and via array".to_string(),
        "thermal interface material, thickness and mounting pressure for the alternate".to_string(),
        "measured case-to-sink resistance on the actual alternate assembly".to_string(),
        "airflow and enclosure boundary conditions for the alternate".to_string(),
    ];
    request.boundary_conditions = vec![
        "case-to-sink interface resistance is a required thermal boundary condition".to_string(),
        "junction temperature must stay below the maximum rated junction temperature".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-same-copper-area",
        "The alternate has the same copper area and MOSFET but a different pad and interface.",
    )];
    request.tests = vec![falsifiable(
        "The alternate PCB is thermally equivalent because it has the same copper area and MOSFET.",
        "Measure case-to-sink thermal resistance and junction temperature on the actual alternate assembly at the specified mounting pressure, power and airflow; repeat at worst-case ambient.",
        "The alternate assembly meets the junction-temperature margin with a measured interface resistance no worse than the assumed value.",
        "The alternate pad raises interface resistance or junction temperature beyond the margin, or the interface cannot be characterized.",
        "JEDEC JESD51-14 thermal tester, thermocouples/TSP, torque-controlled mounting fixture.",
        &[
            "the alternate pad geometry and interface material are different",
            "no interface resistance evidence exists for the alternate",
        ],
    )];
    request.counterexamples = vec![counterexample(
        "zero interface resistance",
        "alternate mounting",
    )];
    request.realization = vec![realization(
        RealizationCategory::Manufacturing,
        "Pad geometry and mounting pressure must be controlled or characterized.",
    )];
    build(
        PCB_ID,
        request,
        vec![
            "prop-contact-rth".to_string(),
            "rule-contact-resistance".to_string(),
            "model-conduction-thermal".to_string(),
        ],
        thermal_controls()?,
    )
}

fn emc() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let authority = authority_of(&registry, &["emc-pcb"])?;
    let binding = model_binding(&registry, "emc-pcb", "model-pcb-loop-inductance")?;
    let requirements = requirement_set(
        "emc-return-path",
        "Establish the decoupling return-loop inductance and spacing on the actual stackup.",
        vec![numeric_requirement(
            "req-emc-loop",
            "Return-path loop inductance stays below the target.",
            engineering_core::Discipline::Emc,
            "loop_inductance",
            engineering_core::NumericLimit::Max {
                limit: q(10.0, "nH")?,
            },
            Some("model-pcb-loop-inductance"),
        )],
        vec![binding],
    );
    let mut request = base_request(
        EMC_ID,
        requirements,
        vec![selection(
            "req-emc-loop",
            engineering_core::Discipline::Emc,
            "loop_inductance",
        )],
        authority,
    );
    request.probes = vec![emc_probe("emc-far-placement", 10.0, 0.5, 0.5)?];
    request.unknowns = vec![
        "revision-bound PCB stackup, dielectric height and copper thickness".to_string(),
        "actual capacitor distance, via length and return-path continuity".to_string(),
        "project clearance and creepage rules with their source".to_string(),
        "plane splits, connector breakout and high-frequency via transitions".to_string(),
    ];
    request.boundary_conditions = vec![
        "trace width must be at least the dielectric height for the loop estimate".to_string(),
        "the loop inductance target is 10 nH at the actual placement".to_string(),
        "clearance and creepage must meet the project rules".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-same-copper-area",
        "The shallow claim treats equal copper area as EMC equivalence and omits placement distance.",
    )];
    request.tests = vec![falsifiable(
        "The alternate layout has the same copper area, so its EMC behaviour is equivalent.",
        "Measure the power/ground loop impedance with a two-port probe or VNA on the actual stackup, and measure clearance/creepage on the fabricated board against the project rules.",
        "Loop inductance is at or below the target at the actual placement and both spacing checks pass.",
        "Loop inductance exceeds the target, placement distance dominates the loop, or clearance/creepage is below the rule.",
        "Two-port impedance probe or VNA, calibrated near-field scan, optical clearance measurement.",
        &[
            "a 10 mm capacitor distance gives 22 mm of loop length and 18.4 nH against a 10 nH target",
            "the adjacent placement is 11 times lower in inductance",
            "0.5 mm clearance and creepage are below the 0.6 mm project rule",
        ],
    )];
    request.counterexamples = vec![
        counterexample("equal copper area", "10 mm decoupling distance"),
        counterexample("adjacent placement", "same stackup control"),
    ];
    request.realization = vec![realization(
        RealizationCategory::Manufacturing,
        "Stackup height, capacitor placement and spacing rules must be controlled on the fabricated board.",
    )];
    build(
        EMC_ID,
        request,
        vec![
            "prop-stackup".to_string(),
            "rule-decoupling-distance".to_string(),
            "rule-spacing".to_string(),
            "trap-same-copper-area".to_string(),
            "model-pcb-loop-inductance".to_string(),
        ],
        emc_controls()?,
    )
}

fn aluminium() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let text: String = registry
        .packs()
        .filter_map(|pack| serde_json::to_string(pack).ok())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if text.contains("plate-t651") || text.contains("yield_strength") || text.contains("250 mpa") {
        return Err(case_error(
            ALUMINIUM_ID,
            "a bundled pack now contains a revision-bound T651 property record; research-required is stale",
        ));
    }
    let authority = authority_of(&registry, &["materials-conditional"])?;
    let requirements = requirement_set(
        "aluminium-strength",
        "Establish 6061-T651 plate strength at 80 mm.",
        vec![numeric_requirement(
            "req-aluminium-yield",
            "Yield strength meets the design allowable.",
            engineering_core::Discipline::Mechanical,
            "yield_strength",
            engineering_core::NumericLimit::Min {
                limit: q(200.0, "MPa")?,
            },
            None,
        )],
        Vec::new(),
    );
    let mut request = base_request(
        ALUMINIUM_ID,
        requirements,
        vec![selection(
            "req-aluminium-yield",
            engineering_core::Discipline::Mechanical,
            "yield_strength",
        )],
        authority,
    );
    let missing = vec![
        "revision-bound yield/tensile property record for 6061-T651 plate at 80 mm".to_string(),
        "temper designation confirmed on the mill certificate".to_string(),
        "product form, thickness and test direction/orientation".to_string(),
        "specification and acceptance basis (for example ASTM B209/B557 or an equivalent licensed source)".to_string(),
        "property uncertainty and lot-to-lot scatter".to_string(),
    ];
    request.unknowns = missing.clone();
    request.boundary_conditions = vec![
        "temper designation must match the qualified material condition (T651, not T6)".to_string(),
        "the property must apply to the actual product form and thickness (80 mm plate, not thin sheet)".to_string(),
        "test direction/orientation and service temperature must be inside the property's applicability".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-any-6061",
        "The shallow claim reuses a familiar 6061 strength for any temper and thickness.",
    )];
    request.tests = vec![falsifiable(
        "6061-T651 plate at 80 mm has adequate strength for the design load.",
        "Cut specimens from the actual plate at the required location and orientation and perform tensile tests at the service temperature; compare Rp0.2/Rm and elongation with the design allowables.",
        "The measured minimum properties, with the lot scatter, meet the design allowables with the required margin.",
        "The measured properties fall below the design allowables, or the temper/thickness/test direction does not match the qualified condition.",
        "Calibrated universal tensile machine, extensometer, mill certificate, ASTM B557/E8 fixtures.",
        &[
            "no revision-bound 6061-T651 plate record is bundled",
            "the candidate reference is T6 thin sheet, not T651 plate",
            "a conditional lookup rule without a property record cannot establish a value",
        ],
    )];
    request.counterexamples = vec![counterexample(
        "T6 thin-sheet substitution",
        "T651 plate at 80 mm",
    )];
    request.realization = vec![realization(
        RealizationCategory::Sourcing,
        "A mill certificate for the actual temper and thickness is required.",
    )];
    request.blocker = Some(PipelineBlocker::Research(ResearchRequest {
        gate: GateId::Authority,
        reason: "no revision-bound property record covers 6061-T651 at 80 mm; the bundled pack supplies a conditional lookup rule but no applicable record".to_string(),
        sources_required: missing,
    }));
    build(ALUMINIUM_ID, request, Vec::new(), Vec::new())
}

fn fastener() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let authority = authority_of(&registry, &["bolted-joint"])?;
    let binding = model_binding(&registry, "bolted-joint", BOLT_MODEL)?;
    let requirements = requirement_set(
        "fastener-preload",
        "Establish preload with nut-factor scatter inside its qualified domain.",
        vec![numeric_requirement(
            "req-fastener-utilization",
            "Bolt utilization stays below the proof-strength limit.",
            engineering_core::Discipline::Mechanical,
            "utilization",
            engineering_core::NumericLimit::Max {
                limit: q(1.0, "1")?,
            },
            Some(BOLT_MODEL),
        )],
        vec![binding],
    );
    let mut request = base_request(
        FASTENER_ID,
        requirements,
        vec![selection(
            "req-fastener-utilization",
            engineering_core::Discipline::Mechanical,
            "preload",
        )],
        authority,
    );
    request.probes = vec![fastener_probe("fastener-invalid-scatter", 0.05, 0.50)?];
    request.unknowns = vec![
        "joint-specific qualified nut-factor distribution".to_string(),
        "production lubrication, coating and thread condition".to_string(),
        "torque-tension test evidence spanning the production K range".to_string(),
    ];
    request.boundary_conditions = vec![
        "nut-factor scatter must lie inside the qualified validity interval [0.12, 0.28]"
            .to_string(),
        "preload is only bounded when the K interval is valid".to_string(),
        "bolt stress utilization must remain below the proof-strength limit".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-nominal-k",
        "The fixture nominal nut factor is 0.2 with an invalid scatter interval.",
    )];
    request.tests = vec![falsifiable(
        "A nominal nut factor of 0.2 predicts the production preload within the required scatter.",
        "Run a torque-tension test on the actual joint with the production lubrication, coating, thread condition and tightening tool; measure clamp force and derive K at the specified torque.",
        "The measured K distribution lies inside the qualified validity range and the resulting preload scatter meets the joint requirement.",
        "The measured K distribution falls outside the qualified range, or the preload scatter exceeds the design allowance.",
        "Calibrated torque-tension rig, load cell, torque transducer, thread-condition records.",
        &[
            "the invalid scatter interval 0.05..0.50 lies outside the qualified K domain 0.12..0.28",
            "the model refuses rather than clamping K or reporting the nominal preload as certain",
        ],
    )];
    request.counterexamples = vec![counterexample(
        "K = 0.2 point estimate",
        "scatter outside the qualified domain",
    )];
    request.realization = vec![realization(
        RealizationCategory::Manufacturing,
        "Production lubrication and thread condition must be controlled.",
    )];
    build(
        FASTENER_ID,
        request,
        vec![
            "prop-nut-factor".to_string(),
            "val-k-domain".to_string(),
            "trap-unrealistic-k".to_string(),
        ],
        fastener_controls()?,
    )
}

fn fatigue() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let text: String = registry
        .packs()
        .filter_map(|pack| serde_json::to_string(pack).ok())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if text.contains("stress_coefficient") || text.contains("endurance_limit") {
        return Err(case_error(
            FATIGUE_ID,
            "a bundled pack now contains S-N curve data; research-required is stale",
        ));
    }
    let authority = authority_of(&registry, &["fatigue-materials"])?;
    let requirements = requirement_set(
        "fatigue-life",
        "Establish infinite life at the service stress and target cycles.",
        vec![numeric_requirement(
            "req-fatigue-life",
            "The component survives the target cycle count without failure.",
            engineering_core::Discipline::Reliability,
            "fatigue_life",
            engineering_core::NumericLimit::Min {
                limit: q(2.0e7, "1")?,
            },
            None,
        )],
        Vec::new(),
    );
    let mut request = base_request(
        FATIGUE_ID,
        requirements,
        vec![selection(
            "req-fatigue-life",
            engineering_core::Discipline::Reliability,
            "fatigue_life",
        )],
        authority,
    );
    let missing = vec![
        "S-N curve with stress ratio, mean stress correction and run-out definition".to_string(),
        "surface finish, notch factor and residual-stress condition".to_string(),
        "environment and temperature for the service regime".to_string(),
        "evidence that the required cycle count is inside the curve's tested validity regime"
            .to_string(),
        "lot/process scatter and the statistical basis for the endurance limit".to_string(),
    ];
    request.unknowns = missing.clone();
    request.boundary_conditions = vec![
        "stress ratio, mean stress and surface condition define the S-N applicability".to_string(),
        "the target cycle count must be inside the curve's tested range and run-out definition"
            .to_string(),
        "the endurance limit cannot be extrapolated from UTS".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-half-uts",
        "The shallow claim takes the endurance limit as half UTS and ignores the S-N life.",
    )];
    request.tests = vec![falsifiable(
        "The component has infinite fatigue life at the service stress and 2e7 cycles.",
        "Run constant-amplitude fatigue tests at the service stress ratio and surface condition to the target cycle count, with additional staircase tests for the endurance limit; inspect failures and run-outs.",
        "All specimens reach the target with no failure and the staircase endurance limit exceeds the service stress with the required margin.",
        "A specimen fails before the target, or the endurance limit is below the service stress, or the target lies outside the tested regime.",
        "Servo-hydraulic or electromagnetic fatigue machine, calibrated load cell, fracture inspection.",
        &[
            "the bundled fatigue pack supplies a model but no S-N curve record",
            "the required 2e7-cycle regime cannot be established without curve data",
            "half-UTS is not an S-N curve",
        ],
    )];
    request.counterexamples = vec![counterexample(
        "0.5 UTS endurance-limit rule",
        "2e7 cycles at the service stress ratio",
    )];
    request.realization = vec![realization(
        RealizationCategory::Reliability,
        "A fatigue test programme is required before the life claim.",
    )];
    request.blocker = Some(PipelineBlocker::Research(ResearchRequest {
        gate: GateId::Validity,
        reason: "the bundled fatigue pack supplies the model but no S-N curve data for the service stress ratio, surface condition and 2e7-cycle target".to_string(),
        sources_required: missing,
    }));
    build(FATIGUE_ID, request, Vec::new(), Vec::new())
}

fn thermal() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let authority = authority_of(&registry, &["power-electronics"])?;
    let binding = model_binding(&registry, "power-electronics", MOSFET_MODEL)?;
    let requirements = requirement_set(
        "thermal-contact",
        "Establish junction temperature with a sourced interface resistance.",
        vec![numeric_requirement(
            "req-thermal-tj",
            "Junction temperature stays below the rated maximum.",
            engineering_core::Discipline::Thermal,
            "junction_temperature",
            engineering_core::NumericLimit::Max {
                limit: q(150.0, "degC")?,
            },
            Some(MOSFET_MODEL),
        )],
        vec![binding],
    );
    let mut request = base_request(
        THERMAL_ID,
        requirements,
        vec![selection(
            "req-thermal-tj",
            engineering_core::Discipline::Thermal,
            "junction_temperature",
        )],
        authority,
    );
    request.probes = vec![thermal_probe("thermal-missing-contact", false)?];
    request.unknowns = vec![
        "revision-bound thermal interface material data at the assembly pressure".to_string(),
        "measured case-to-sink resistance for the actual stack".to_string(),
    ];
    request.boundary_conditions = vec![
        "Rth_cs must be strictly positive; a missing or zero interface is not a valid boundary condition".to_string(),
        "junction temperature must stay below the maximum rated junction temperature".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-interface-negligible",
        "The shallow claim treats the interface resistance as negligible.",
    )];
    request.tests = vec![falsifiable(
        "The junction-to-case and sink-to-ambient resistances alone determine the junction temperature.",
        "Measure the case-to-sink interface resistance and junction temperature on the actual assembly at the production mounting pressure and worst-case ambient; compare with the assumed thermal chain.",
        "The measured interface resistance is no greater than the value used in the calculation and the junction-temperature margin remains positive.",
        "The interface resistance or junction temperature exceeds the calculated value, or the interface is uncharacterized.",
        "JEDEC JESD51-14 tester, thermal test die or TSP, calibrated thermocouples, torque-controlled fixture.",
        &[
            "the missing-interface case is refused rather than assigned Rth_cs = 0",
            "a supplied zero interface resistance is rejected as non-positive",
        ],
    )];
    request.counterexamples = vec![counterexample(
        "Rth_cs = 0 substitution",
        "actual mounting pressure",
    )];
    request.realization = vec![realization(
        RealizationCategory::Manufacturing,
        "Interface material and mounting pressure must be controlled.",
    )];
    build(
        THERMAL_ID,
        request,
        vec![
            "prop-contact-rth".to_string(),
            "rule-contact-resistance".to_string(),
            "trap-missing-contact-resistance".to_string(),
        ],
        thermal_controls()?,
    )
}

fn molding() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let authority = authority_of(&registry, &["injection-molding"])?;
    let binding = model_binding(&registry, "injection-molding", MOLDING_MODEL)?;
    let unit = |value: f64, unit: &str| -> Result<Quantity, BenchError> { q(value, unit) };
    let requirements = requirement_set(
        "molding-dfm",
        "Establish draft, wall and flow manufacturability.",
        vec![
            numeric_requirement(
                "req-molding-draft",
                "Draft margin is non-negative.",
                engineering_core::Discipline::Manufacturing,
                "draft_margin",
                engineering_core::NumericLimit::Min {
                    limit: unit(0.0, "deg")?,
                },
                Some(MOLDING_MODEL),
            ),
            numeric_requirement(
                "req-molding-wall",
                "Wall variation is inside the DFM limit.",
                engineering_core::Discipline::Manufacturing,
                "wall_variation",
                engineering_core::NumericLimit::Max {
                    limit: unit(0.12, "1")?,
                },
                Some(MOLDING_MODEL),
            ),
            numeric_requirement(
                "req-molding-flow",
                "Flow length ratio is inside the DFM limit.",
                engineering_core::Discipline::Manufacturing,
                "flow_ratio",
                engineering_core::NumericLimit::Max {
                    limit: unit(150.0, "1")?,
                },
                Some(MOLDING_MODEL),
            ),
        ],
        vec![binding],
    );
    let mut request = base_request(
        MOLDING_ID,
        requirements,
        vec![
            selection(
                "req-molding-draft",
                engineering_core::Discipline::Manufacturing,
                "draft_margin",
            ),
            selection(
                "req-molding-wall",
                engineering_core::Discipline::Manufacturing,
                "wall_variation",
            ),
            selection(
                "req-molding-flow",
                engineering_core::Discipline::Manufacturing,
                "flow_ratio",
            ),
        ],
        authority,
    );
    request.probes = vec![molding_probe(
        "molding-bad-draft-wall",
        0.5,
        "deg",
        2.5,
        2.2,
        2.9,
        400.0,
    )?];
    request.unknowns = vec![
        "material/tool-specific minimum draft and wall limits".to_string(),
        "texture depth and ejection design".to_string(),
    ];
    request.boundary_conditions = vec![
        "draft_actual >= draft_min_required".to_string(),
        "wall_variation <= max_wall_variation".to_string(),
        "flow_ratio <= max_flow_ratio".to_string(),
    ];
    request.assumptions = vec![assumption(
        "asm-rule-inputs",
        "Minimum draft, maximum wall variation and maximum flow ratio are explicit DFM inputs.",
    )];
    request.tests = vec![falsifiable(
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
    )];
    request.counterexamples = vec![
        counterexample("0.5 deg draft", "1.0 deg requirement"),
        counterexample("0.5 rad draft", "unit-confusion control"),
    ];
    request.realization = vec![realization(
        RealizationCategory::Manufacturing,
        "Tool texture, draft and wall limits must be verified on the first article.",
    )];
    build(
        MOLDING_ID,
        request,
        vec![
            "rule-draft".to_string(),
            "rule-wall-uniformity".to_string(),
            "rule-flow-ratio".to_string(),
            "trap-draft-units".to_string(),
        ],
        molding_controls()?,
    )
}

fn fea() -> Result<SutAnswer, BenchError> {
    let registry = registry()?;
    let gmsh = GmshSolver::new();
    let capabilities = gmsh.capabilities();
    let mesh_only = capabilities.analyses == vec![AnalysisRequest::Mesh3d]
        && capabilities.input_formats == vec!["geo".to_string()];
    if !mesh_only {
        return Err(case_error(
            FEA_ID,
            "the Gmsh adapter now claims a structural analysis; the refusal label is stale",
        ));
    }
    let mesh = crate::fixtures::mesh_fixture_probe();
    if !mesh.ok() {
        return Err(case_error(
            FEA_ID,
            format!("mesh fixture probe failed: {mesh:?}"),
        ));
    }
    let study = crate::fixtures::manufactured_solution_study()
        .map_err(|error| case_error(FEA_ID, error))?;
    let errors = study.iter().map(|(_, error)| *error).collect::<Vec<_>>();
    if crate::fixtures::convergence_pathology(&errors).is_some() {
        return Err(case_error(
            FEA_ID,
            "the manufactured-solution control is pathological",
        ));
    }
    if crate::fixtures::convergence_pathology(&[0.40, 0.35, 0.36, 0.22]).is_none()
        || crate::fixtures::convergence_pathology(&[0.40, 0.20, 0.10, 0.095]).is_none()
    {
        return Err(case_error(
            FEA_ID,
            "a pathological refinement control was not detected",
        ));
    }
    let authority = authority_of(&registry, &["passives"])?;
    let requirements = requirement_set(
        "fea-convergence",
        "Establish mesh-independent structural stress.",
        vec![categorical_requirement(
            "req-fea-convergence",
            "The quantity of interest is mesh-independent and converged.",
            engineering_core::Discipline::Mechanical,
            "mesh_convergence",
            &["converged"],
        )],
        Vec::new(),
    );
    let mut request = base_request(
        FEA_ID,
        requirements,
        vec![selection(
            "req-fea-convergence",
            engineering_core::Discipline::Mechanical,
            "mesh_convergence",
        )],
        authority,
    );
    let required = vec![
        "structural solver with a convergence residual and tolerance".to_string(),
        "three-level mesh-independence study of the quantity of interest".to_string(),
        "element-quality metrics and boundary conditions".to_string(),
        "independent reference or analytical limit for the quantity of interest".to_string(),
    ];
    request.unknowns = vec![
        "mesh-independence study and convergence tolerance".to_string(),
        "element quality, aspect ratio and boundary conditions".to_string(),
        "independent reference solution or experimental validation".to_string(),
    ];
    request.boundary_conditions = vec![
        "refinement must be monotone and reduce the error".to_string(),
        "the observed order must be consistent with the discretization".to_string(),
        "mesh counts and files are bounded before parsing".to_string(),
    ];
    request.tests = vec![falsifiable(
        "The reported stress is mesh-independent and the solver has converged.",
        "Refine the mesh over at least three levels and track the quantity of interest; compare the finest result with an independent reference or analytical limit and record the observed order and residual.",
        "The quantity changes by less than the stated tolerance over the last levels, the refinement is monotone, and the observed order is consistent with the discretization.",
        "The quantity does not converge, oscillates, stalls, or changes by more than the tolerance under refinement.",
        "Mesh generator, structural solver with convergence residuals, independent analytical/reference model.",
        &[
            "the bundled Gmsh adapter is mesh-generation only (AnalysisRequest::Mesh3d)",
            "a solver exit code and element count are not a convergence study",
            "a manufactured-solution Poisson study shows second-order error reduction in the control",
        ],
    )];
    request.counterexamples = vec![
        counterexample("non-monotone refinement", "pathological control"),
        counterexample("stalled refinement", "pathological control"),
        counterexample("truncated mesh", "parser control"),
    ];
    request.realization = vec![realization(
        RealizationCategory::Test,
        "A structural solver and mesh-independence study are required before stress is reported.",
    )];
    request.blocker = Some(PipelineBlocker::Refusal(engineering_core::KernelRefusal {
        code: "MeshConvergenceNotEstablished".to_string(),
        reason: "the bundled adapter only generates meshes; no structural solve/convergence receipt exists, and the pathological refinement controls are refused".to_string(),
        required,
    }));
    build(
        FEA_ID,
        request,
        vec!["mesh-only".to_string(), "gmsh".to_string()],
        Vec::new(),
    )
}

pub(crate) fn run(id: &str) -> Result<SutAnswer, BenchError> {
    let answer = match id {
        OPAMP_ID => opamp()?,
        LARGE_SIGNAL_ID => large_signal()?,
        FLYBACK_ID => flyback()?,
        PCB_ID => pcb()?,
        ALUMINIUM_ID => aluminium()?,
        FASTENER_ID => fastener()?,
        FATIGUE_ID => fatigue()?,
        THERMAL_ID => thermal()?,
        MOLDING_ID => molding()?,
        FEA_ID => fea()?,
        EMC_ID => emc()?,
        other => return Err(BenchError::UnknownCase(other.to_string())),
    };
    if answer.id != id {
        return Err(case_error(id, "SUT answer id mismatch"));
    }
    Ok(answer)
}
