use engineering_core::{
    Agreement, AnalysisIdentity, AnalysisKind, AnalysisReceipt, AnalysisWarning, AssumptionRecord,
    AuthorityRef, Convergence, CounterexampleProbe, CounterexampleSearch, CrossCheck, Digest,
    DimensionCheck, DimensionChecks, EngineeringResponse, ExecutedAnalysis, FalsifiableTest,
    FalsifiableVerification, Formulation, Gate, GateId, KnowledgeAuthority, ModelSelection, NodeId,
    ProbeOutcome, RealizationCategory, RealizationConsequence, RealizationConsequences,
    ReceiptContent, ReceiptResult, ReceiptValue, Repeatability, RevisionId, TheorySelection,
    UncertaintyContribution, UncertaintyStatement, ValidityEnvelope, ValidityEnvelopes, ValueState,
    WarningSeverity,
};
use engineering_units::{Dimension, Interval, Quantity, Uncertainty, UnitRegistry};

fn provenance() -> engineering_core::Provenance {
    engineering_core::Provenance::new(
        engineering_core::ProvenanceKind::Calculation,
        "gate test fixture",
    )
    .unwrap()
}

fn receipt(solver_id: &str) -> AnalysisReceipt {
    let mut content = ReceiptContent::new(
        Digest::of_text("input"),
        Digest::of_text("model"),
        Digest::of_text("settings"),
    );
    content.convergence = Convergence::exact();
    content.warnings = vec![AnalysisWarning {
        code: "fixture".to_string(),
        message: "synthetic gate fixture".to_string(),
        severity: WarningSeverity::Info,
    }];
    content.results = vec![ReceiptResult {
        name: "result".to_string(),
        value: ReceiptValue::Quantity(
            Quantity::new(1.0, UnitRegistry::si().require("N").unwrap()).unwrap(),
        ),
        uncertainty: Uncertainty::Exact,
    }];
    content.repeatability = Repeatability::deterministic();
    AnalysisReceipt::kernel(
        AnalysisIdentity {
            solver_id: solver_id.to_string(),
            solver_version: "1.0.0".to_string(),
            kind: AnalysisKind::InProcessKernel,
        },
        content,
        0,
    )
    .unwrap()
}

fn valid_response() -> EngineeringResponse {
    let source = provenance();
    let generation = Digest::of_text("fixture-pack");
    let model = receipt("kernel-a");
    let independent = receipt("kernel-b");
    EngineeringResponse {
        formulation: Gate::established(Formulation {
            objective: "check a bolted joint".to_string(),
            scope: "starter fixture".to_string(),
            requirements: vec![NodeId::new(1)],
            constraints: vec![NodeId::new(2)],
            assumptions: vec![AssumptionRecord {
                id: "asm-1".to_string(),
                statement: "quasi-static tightening".to_string(),
                provenance: source.clone(),
            }],
            unknowns: vec!["embedment".to_string()],
        }),
        authority: Gate::established(KnowledgeAuthority {
            packs: vec![AuthorityRef {
                pack_id: "bolted-joint".to_string(),
                version: "1.0.0".to_string(),
                digest: generation,
                scope: "machine_design".to_string(),
            }],
        }),
        theory: Gate::established(TheorySelection {
            models: vec![ModelSelection {
                model_id: "torque-preload".to_string(),
                version: "1.0.0".to_string(),
                equation_ids: vec!["eq-torque-preload".to_string()],
                rationale: "governing scalar relation for this regime".to_string(),
            }],
            alternatives_considered: vec!["thread-strip model (different regime)".to_string()],
        }),
        validity: Gate::established(ValidityEnvelopes {
            envelopes: vec![ValidityEnvelope {
                model_id: "torque-preload".to_string(),
                temperature: Some(Interval::new(233.15, 573.15).unwrap()),
                conditions: vec!["qualified nut factor".to_string()],
                exclusions: vec!["gross yielding".to_string()],
            }],
        }),
        dimensions: Gate::established(DimensionChecks {
            checks: vec![
                DimensionCheck::new("T = K F d", Dimension::ENERGY, Dimension::ENERGY).unwrap(),
                DimensionCheck::new("F = T / (K d)", Dimension::FORCE, Dimension::FORCE).unwrap(),
            ],
        }),
        executed_analysis: Gate::established(ExecutedAnalysis {
            receipts: vec![model],
        }),
        cross_check: Gate::established(CrossCheck {
            method: "independent back-substitution".to_string(),
            independent_of: "kernel-a".to_string(),
            receipts: vec![independent],
            agreement: Agreement {
                metric: "absolute difference".to_string(),
                value: 0.0,
                tolerance: 1e-6,
                agrees: true,
            },
        }),
        uncertainty: Gate::established(UncertaintyStatement {
            method: "explicit interval propagation".to_string(),
            contributions: vec![UncertaintyContribution {
                source: "nut-factor scatter".to_string(),
                uncertainty: Uncertainty::Interval(Interval::new(0.16, 0.24).unwrap()),
                note: "joint-specific interval".to_string(),
            }],
            coverage: Some(1.0),
            summary: Some(Interval::new(0.16, 0.24).unwrap()),
        }),
        counterexample_search: Gate::established(CounterexampleSearch {
            probes: vec![CounterexampleProbe {
                probe: "drive K outside its qualified range".to_string(),
                regime: "validity boundary".to_string(),
                outcome: ProbeOutcome::NoFailureFound,
                evidence: vec!["refusal path executed".to_string()],
            }],
        }),
        realization: Gate::established(RealizationConsequences {
            consequences: vec![RealizationConsequence {
                category: RealizationCategory::Manufacturing,
                statement: "lubrication condition must be controlled".to_string(),
                availability: ValueState::Known("process control available".to_string()),
                evidence: vec!["assembly control plan".to_string()],
            }],
        }),
        verification: Gate::established(FalsifiableVerification {
            tests: vec![FalsifiableTest {
                claim: "preload lies inside the stated interval".to_string(),
                procedure: "torque and clamp-force measurement".to_string(),
                pass_criterion: "measured preload inside interval".to_string(),
                refutation: "measurement outside interval refutes the model".to_string(),
                instrument: "calibrated load cell".to_string(),
                evidence: vec!["test plan".to_string()],
            }],
        }),
    }
}

#[test]
fn a_fully_backed_response_is_ready() {
    let response = valid_response();
    let assessment = response.assess();
    assert!(assessment.is_ready(), "{assessment:?}");
    assert_eq!(assessment.established_gates, 11);
}

#[test]
fn gate_five_requires_machine_checked_dimensions() {
    let mut response = valid_response();
    response.dimensions = Gate::established(DimensionChecks { checks: vec![] });
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Dimensions));
}

#[test]
fn gate_five_rejects_a_dimensionally_inconsistent_equation() {
    let mut response = valid_response();
    response.dimensions = Gate::established(DimensionChecks {
        checks: vec![DimensionCheck::new("T = K F", Dimension::ENERGY, Dimension::FORCE).unwrap()],
    });
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Dimensions));
}

#[test]
fn gate_six_requires_an_executed_receipt_not_prose() {
    let mut response = valid_response();
    response.executed_analysis = Gate::established(ExecutedAnalysis { receipts: vec![] });
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::ExecutedAnalysis));

    let mut lying = receipt("kernel-a");
    lying.executed = false;
    let mut response = valid_response();
    response.executed_analysis = Gate::established(ExecutedAnalysis {
        receipts: vec![lying],
    });
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::ExecutedAnalysis
            && violation.message.contains("invalid receipt")));
}

#[test]
fn unavailable_theory_becomes_typed_research_not_improvisation() {
    let mut response = valid_response();
    response.theory = Gate::unavailable(
        "no validated model for creep at the operating temperature",
        vec!["validated creep model".to_string()],
    );
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    let request = assessment
        .research_required
        .iter()
        .find(|request| request.gate == GateId::Theory)
        .expect("theory research request");
    assert_eq!(request.sources_required, vec!["validated creep model"]);
}

#[test]
fn unavailable_validity_becomes_typed_research() {
    let mut response = valid_response();
    response.validity = Gate::unavailable(
        "no source establishes the temperature envelope",
        vec!["revision-bound datasheet".to_string()],
    );
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .research_required
        .iter()
        .any(|request| request.gate == GateId::Validity));
}

#[test]
fn cross_check_must_be_independent_of_the_primary_solver() {
    let mut response = valid_response();
    response.cross_check = Gate::established(CrossCheck {
        method: "same implementation again".to_string(),
        independent_of: "kernel-a".to_string(),
        receipts: vec![receipt("kernel-a")],
        agreement: Agreement {
            metric: "difference".to_string(),
            value: 0.0,
            tolerance: 1e-6,
            agrees: true,
        },
    });
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::CrossCheck));
}

#[test]
fn a_decision_gate_without_assumption_provenance_is_refused() {
    let mut response = valid_response();
    if let engineering_core::GateState::Established(formulation) = &mut response.formulation.state {
        formulation.assumptions[0].provenance.citation = " ".to_string();
    }
    let assessment = response.assess();
    assert!(!assessment.is_ready());
}

#[test]
fn an_unknown_solver_version_cannot_establish_execution() {
    let mut response = valid_response();
    if let engineering_core::GateState::Established(executed) =
        &mut response.executed_analysis.state
    {
        executed.receipts[0].identity.solver_version = "1.0.0+tool/unknown".to_string();
    }
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment.violations.iter().any(|violation| {
        violation.gate == GateId::ExecutedAnalysis
            && violation.message.contains("unknown solver version")
    }));
}

#[test]
fn oversized_untrusted_text_cannot_pass_a_gate() {
    let mut response = valid_response();
    if let engineering_core::GateState::Established(verification) = &mut response.verification.state
    {
        verification.tests[0].evidence = vec!["x".repeat(2_048)];
    }
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Verification));

    let mut response = valid_response();
    if let engineering_core::GateState::Established(formulation) = &mut response.formulation.state {
        formulation.objective = "x".repeat(16_384);
    }
    let assessment = response.assess();
    assert!(!assessment.is_ready());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Formulation));
}

#[test]
fn the_gate_ids_are_the_eleven_contract_steps() {
    assert_eq!(GateId::ALL.len(), 11);
    assert_eq!(GateId::ALL[0], GateId::Formulation);
    assert_eq!(GateId::ALL[10], GateId::Verification);
    assert_eq!(RevisionId::new(1).get(), 1);
}
