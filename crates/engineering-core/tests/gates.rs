use engineering_core::{
    Agreement, AnalysisIdentity, AnalysisKind, AnalysisReceipt, AnalysisWarning, AssumptionRecord,
    AuthorityRef, CatalogModel, Convergence, CounterexampleProbe, CounterexampleSearch, CrossCheck,
    Digest, DimensionCheck, DimensionChecks, Discipline, EngineeringRequirement,
    EngineeringRequirementSet, EngineeringResponse, ExecutedAnalysis, FalsifiableTest,
    FalsifiableVerification, Formulation, Gate, GateId, KnowledgeAuthority, ModelBinding,
    ModelCatalog, ModelSelection, ModelSelector, ModelValidityOverride, NodeId, NumericLimit,
    ProbeOutcome, RealizationCategory, RealizationConsequence, RealizationConsequences,
    ReceiptContent, ReceiptResult, ReceiptSigner, ReceiptValue, Repeatability, RequirementTarget,
    RevisionId, SelectionProvenance, SelectionRequest, TheorySelection, UncertaintyContribution,
    UncertaintyStatement, ValidityDomain, ValidityEnvelopes, ValueState, WarningSeverity,
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
    let mut receipt = AnalysisReceipt::kernel(
        AnalysisIdentity {
            solver_id: solver_id.to_string(),
            solver_version: "1.0.0".to_string(),
            kind: AnalysisKind::InProcessKernel,
        },
        content,
        0,
    )
    .unwrap();
    ReceiptSigner::from_seed([7u8; 32])
        .sign_receipt(&mut receipt)
        .unwrap();
    receipt
}

fn selection() -> engineering_core::ApplicabilitySelection {
    let catalog = ModelCatalog::new(vec![CatalogModel {
        model_id: "torque-preload".to_string(),
        source_model_id: None,
        version: "1.0.0".to_string(),
        pack_id: Some("bolted-joint".to_string()),
        pack_version: Some("1.0.0".to_string()),
        pack_digest: Some(Digest::of_text("fixture-pack")),
        equation_ids: vec!["eq-torque-preload".to_string()],
        disciplines: vec![Discipline::Mechanical],
        properties: vec!["torque".to_string(), "preload".to_string()],
        domains: vec![ValidityDomain {
            id: "val-fixture".to_string(),
            statement: "fixture validity domain".to_string(),
            temperature: Some(Interval::new(233.15, 573.15).unwrap()),
            conditions: vec!["qualified nut factor".to_string()],
            exclusions: vec!["gross yielding".to_string()],
            materials: vec![],
            provenance: SelectionProvenance::new("reference", "fixture").unwrap(),
        }],
        provenance: SelectionProvenance::new("reference", "fixture").unwrap(),
    }])
    .unwrap();
    ModelSelector::select(
        &catalog,
        &[SelectionRequest {
            requirement_id: "req-preload".to_string(),
            discipline: Discipline::Mechanical,
            property: Some("preload".to_string()),
            conditions: vec!["qualified nut factor".to_string()],
            material: None,
            temperature: Some(
                Quantity::new(300.0, UnitRegistry::si().require("K").unwrap()).unwrap(),
            ),
        }],
    )
    .unwrap()
}

fn override_requirements() -> EngineeringRequirementSet {
    EngineeringRequirementSet {
        id: "set-override".to_string(),
        revision: RevisionId::new(1),
        objective: "check a bolted joint under an approved override".to_string(),
        scope: "one joint".to_string(),
        requirements: vec![EngineeringRequirement {
            id: "req-preload".to_string(),
            statement: "preload is at least 1 kN".to_string(),
            discipline: Discipline::Mechanical,
            mandatory: true,
            target: RequirementTarget::Numeric {
                property: "preload".to_string(),
                limit: NumericLimit::Min {
                    limit: Quantity::new(1_000.0, UnitRegistry::si().require("N").unwrap())
                        .unwrap(),
                },
                model: Some("torque-preload".to_string()),
            },
            conditions: vec![],
            provenance: provenance(),
        }],
        constraints: vec![],
        models: vec![ModelBinding {
            model_id: "torque-preload".to_string(),
            version: "1.0.0".to_string(),
            equation_ids: vec!["eq-torque-preload".to_string()],
            temperature: None,
            conditions: vec![],
            exclusions: vec![],
            pack_id: None,
            pack_version: None,
            provenance: provenance(),
        }],
        evidence: vec![],
    }
}

fn override_selection() -> engineering_core::ApplicabilitySelection {
    let plain = selection();
    let requirements = override_requirements();
    let approval = ModelValidityOverride {
        requirement_set_id: requirements.id.clone(),
        requirement_set_revision: requirements.revision,
        requirement_set_digest: Digest::of_json(&requirements).unwrap(),
        requirement_id: "req-preload".to_string(),
        model_id: "torque-preload".to_string(),
        rationale: "fixture human-approved out-of-envelope use".to_string(),
    };
    let digest = approval.digest(&requirements, &plain.catalog).unwrap();
    let token = faktor_approval::ApprovalToken::test_token(
        faktor_approval::ApprovalKind::EngineeringModelValidityOverride,
        requirements.id.clone(),
        requirements.revision.get().to_string(),
        digest.to_hex(),
    );
    ModelSelector::select_with_override(
        &plain.catalog,
        &plain.requests,
        &requirements,
        Some((&approval, &token)),
    )
    .unwrap()
}

fn resign(mut receipt: AnalysisReceipt) -> AnalysisReceipt {
    receipt.attestation = None;
    ReceiptSigner::from_seed([7u8; 32])
        .sign_receipt(&mut receipt)
        .unwrap();
    receipt
}

fn valid_response() -> EngineeringResponse {
    let source = provenance();
    let generation = Digest::of_text("fixture-pack");
    let model = receipt("kernel-a");
    let independent = receipt("kernel-b");
    let selection = selection();
    let rationale = selection
        .selected_models()
        .next()
        .expect("fixture selection")
        .rationale
        .clone();
    let validity = ValidityEnvelopes {
        envelopes: selection.envelopes().unwrap(),
    };
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
                rationale,
            }],
            alternatives_considered: vec!["thread-strip model (different regime)".to_string()],
            selection: Some(selection),
        }),
        validity: Gate::established(validity),
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
    let assessment = response.assess_integrity_only();
    assert!(assessment.is_self_consistent(), "{assessment:?}");
    assert_eq!(assessment.established_gates, 11);
}

#[test]
fn gate_five_requires_machine_checked_dimensions() {
    let mut response = valid_response();
    response.dimensions = Gate::established(DimensionChecks { checks: vec![] });
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Dimensions));
}

#[test]
fn gate_six_requires_an_executed_receipt_not_prose() {
    let mut response = valid_response();
    response.executed_analysis = Gate::established(ExecutedAnalysis { receipts: vec![] });
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
}

#[test]
fn an_unknown_solver_version_cannot_establish_execution() {
    let mut response = valid_response();
    if let engineering_core::GateState::Established(executed) =
        &mut response.executed_analysis.state
    {
        executed.receipts[0].identity.solver_version = "1.0.0+tool/unknown".to_string();
    }
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Verification));

    let mut response = valid_response();
    if let engineering_core::GateState::Established(formulation) = &mut response.formulation.state {
        formulation.objective = "x".repeat(16_384);
    }
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
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

#[test]
fn an_unsigned_receipt_cannot_establish_execution() {
    let mut response = valid_response();
    if let engineering_core::GateState::Established(executed) =
        &mut response.executed_analysis.state
    {
        executed.receipts[0].attestation = None;
    }
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment.violations.iter().any(|violation| {
        violation.gate == GateId::ExecutedAnalysis && violation.message.contains("attestation")
    }));
}

#[test]
fn a_receipt_signed_by_a_foreign_key_cannot_establish_execution() {
    let response = valid_response();
    let other = ReceiptSigner::from_seed([8u8; 32]);
    let verifier = engineering_core::ReceiptVerifier::trusting_key_id(&other.key_id()).unwrap();
    let assessment = response.assess_with_verifier(&verifier);
    assert!(!assessment.is_self_consistent());
    assert!(assessment.violations.iter().any(|violation| {
        violation.gate == GateId::ExecutedAnalysis && violation.message.contains("untrusted key")
    }));
    // The fixture is genuinely self-signed: integrity-only names accept it,
    // while the verifier-requiring authority names refuse it.
    assert!(response.is_self_consistent());
    assert!(response.require_self_consistent().is_ok());
    assert!(!response.is_verified(&verifier));
    assert!(response.require_ready(&verifier).is_err());
    let own = ReceiptSigner::from_seed([7u8; 32]);
    let own_verifier = engineering_core::ReceiptVerifier::trusting_key_id(&own.key_id()).unwrap();
    assert!(response.require_ready(&own_verifier).is_ok());
}

#[test]
fn a_model_override_is_bound_into_the_signed_receipt_and_the_gate_path() {
    let plain = selection();
    let overridden = override_selection();
    assert_ne!(
        plain.digest().unwrap(),
        overridden.digest().unwrap(),
        "applying an override must change the signed selection digest"
    );
    assert_eq!(overridden.overrides.len(), 1);
    assert_eq!(
        overridden.overrides[0].selection_digest,
        overridden.selection_core_digest().unwrap()
    );

    let mut response = valid_response();
    let unique = overridden.unique_models();
    let selected = unique[0];
    response.theory = Gate::established(TheorySelection {
        models: vec![ModelSelection {
            model_id: selected.model_id.clone(),
            version: selected.version.clone(),
            equation_ids: selected.equation_ids.clone(),
            rationale: selected.rationale.clone(),
        }],
        alternatives_considered: vec!["approved override fixture".to_string()],
        selection: Some(overridden.clone()),
    });
    response.validity = Gate::established(ValidityEnvelopes {
        envelopes: overridden.envelopes().unwrap(),
    });
    if let engineering_core::GateState::Established(executed) =
        &mut response.executed_analysis.state
    {
        for receipt in &mut executed.receipts {
            let mut updated = receipt.clone();
            updated.model_overrides = overridden.overrides.clone();
            *receipt = resign(updated);
        }
    }
    if let engineering_core::GateState::Established(cross) = &mut response.cross_check.state {
        for receipt in &mut cross.receipts {
            let mut updated = receipt.clone();
            updated.model_overrides = overridden.overrides.clone();
            *receipt = resign(updated);
        }
    }
    let assessment = response.assess_integrity_only();
    assert!(
        assessment.is_self_consistent(),
        "a fully bound override must satisfy the 11-gate contract: {assessment:?}"
    );

    // An override applied to the response but absent from the signed receipt
    // is refused even if the receipt's own signature is still valid.
    let mut unsigned_override = response.clone();
    if let engineering_core::GateState::Established(executed) =
        &mut unsigned_override.executed_analysis.state
    {
        let mut updated = executed.receipts[0].clone();
        updated.model_overrides.clear();
        executed.receipts[0] = resign(updated);
    }
    let assessment = unsigned_override.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment.violations.iter().any(|violation| {
        violation.gate == GateId::ExecutedAnalysis && violation.message.contains("override binding")
    }));

    // A receipt that carries an override the response did not apply is also
    // refused: override evidence may not be smuggled in on only one side.
    let mut unselected_override = valid_response();
    if let engineering_core::GateState::Established(executed) =
        &mut unselected_override.executed_analysis.state
    {
        let mut updated = executed.receipts[0].clone();
        updated.model_overrides = overridden.overrides.clone();
        executed.receipts[0] = resign(updated);
    }
    let assessment = unselected_override.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::ExecutedAnalysis));

    // Tampering with any override field invalidates the signature.
    let mut tampered = response.clone();
    if let engineering_core::GateState::Established(executed) =
        &mut tampered.executed_analysis.state
    {
        executed.receipts[0].model_overrides[0]
            .override_record
            .rationale = "rewritten after signing".to_string();
        assert!(executed.receipts[0].validate().is_err());
    }
    let assessment = tampered.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::ExecutedAnalysis));
}

#[test]
fn a_caller_cannot_replace_a_selected_validity_envelope() {
    let mut response = valid_response();
    if let engineering_core::GateState::Established(validity) = &mut response.validity.state {
        validity.envelopes[0].temperature = Some(Interval::new(1.0, 10_000.0).unwrap());
    }
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Validity));
}

#[test]
fn a_caller_cannot_substitute_a_model_outside_the_selector() {
    let mut response = valid_response();
    if let engineering_core::GateState::Established(theory) = &mut response.theory.state {
        theory.models[0].model_id = "caller-preferred".to_string();
    }
    let assessment = response.assess_integrity_only();
    assert!(!assessment.is_self_consistent());
    assert!(assessment
        .violations
        .iter()
        .any(|violation| violation.gate == GateId::Theory));
}
