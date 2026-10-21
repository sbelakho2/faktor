use std::collections::BTreeMap;
use std::time::Instant;

use engineering_core::{
    CandidateFacts, CategoricalFact, Discipline, EngineeringRequirement, EngineeringRequirementSet,
    EngineeringVerdict, ModelBinding, NumericLimit, OutcomeState, RequirementEvidence,
    RequirementTarget, RevisionId,
};
use engineering_units::{Quantity, UnitRegistry};
use faktor_supply_core::{
    Availability, CandidateId, Currency, Day, Distribution, EvidenceRef, ExactError, Incoterm,
    InventorySummary, Money, PartId, Provenance as SupplyProvenance, Rate, RecommendedAction,
    Revision, ScaledQuantity, SiteId, SourceKind, SupplierId, SupplierRole, SupplyRequirement,
    TransportMode, Units,
};
use faktor_supply_cost::{
    not_applicable_term, should_cost, ExcessMoq, LaborTime, MachineTime, MarginMode, MaterialLine,
    Overhead, OverheadBase, ProcessStep, QualityCost, ScrapRework, SetupCost, ShouldCostInput,
    TcoInput, WorkingCapital,
};
use faktor_supply_execution::{
    authorize, ActionKind, ActionRequest, AuthorizationGrant, Quote, VolumePrice,
};

use faktor_supply_engineering::*;

const CUR: Currency = Currency::USD;
const DAY: i64 = 20_000;
const REV: u64 = 7;

fn revision() -> RevisionId {
    RevisionId::new(REV)
}

fn eng_provenance(citation: &str) -> engineering_core::Provenance {
    engineering_core::Provenance::new(engineering_core::ProvenanceKind::Specification, citation)
        .unwrap()
        .with_revision(revision())
}

fn quantity(value: f64, unit: &str) -> Quantity {
    Quantity::new(value, UnitRegistry::si().require(unit).unwrap()).unwrap()
}

fn supply_provenance(id: &str) -> SupplyProvenance {
    SupplyProvenance::new(
        SourceKind::SupplierDocument,
        EvidenceRef::new(id, Revision(2), None).unwrap(),
        Day(DAY),
        "fixture",
        vec![],
        "a revised document with a different value",
    )
    .unwrap()
}

fn term<T>(value: T, id: &str) -> faktor_supply_core::Term<T> {
    faktor_supply_core::Term::evidence(value, supply_provenance(id))
}

fn tco_input(
    quantity_units: u64,
    unit_price: &str,
    freight: &str,
    moq: u64,
    defect_ppm: i64,
    payment_days: u32,
) -> TcoInput {
    let units = Units::new(quantity_units).unwrap();
    let price = Money::parse(unit_price, CUR).unwrap();
    let financed = price.mul_units(quantity_units).unwrap();
    let excess = moq.saturating_sub(quantity_units);
    TcoInput {
        currency: CUR,
        quantity: units,
        ex_works_unit: term(price, "tco-ex-works"),
        origin_inland_freight: not_applicable_term("domestic supplier"),
        export_handling: not_applicable_term("domestic supplier"),
        international_freight: term(Money::parse(freight, CUR).unwrap(), "tco-freight"),
        insurance: not_applicable_term("self-insured"),
        import_handling: not_applicable_term("domestic supplier"),
        duty: not_applicable_term("domestic supplier"),
        tax: not_applicable_term("no recoverable tax in the fixture"),
        brokerage: not_applicable_term("domestic supplier"),
        final_mile: not_applicable_term("included in freight"),
        working_capital: term(
            WorkingCapital {
                annual_rate: Rate::new(120_000).unwrap(),
                days: payment_days,
                financed_amount: financed,
            },
            "tco-capital",
        ),
        expected_quality: term(
            QualityCost {
                defect_ppm,
                units,
                cost_per_defect: Money::parse("2.00", CUR).unwrap(),
            },
            "tco-quality",
        ),
        expected_expedite: not_applicable_term("no expedite observed"),
        excess_moq_carrying: if excess > 0 {
            term(
                ExcessMoq {
                    excess_units: Units::new(excess).unwrap(),
                    unit_value: price,
                    annual_carrying_rate: Rate::new(200_000).unwrap(),
                    days: 365,
                },
                "tco-moq",
            )
        } else {
            not_applicable_term("quantity meets MOQ")
        },
        obsolescence: not_applicable_term("active part"),
    }
}

fn quote(id: &str, supplier: u32, price: &str, moq: u64, payment_days: u32) -> Quote {
    Quote {
        id: id.to_string(),
        supplier: SupplierId(supplier),
        part: PartId(1),
        revision: Revision(2),
        currency: CUR,
        prices: vec![VolumePrice {
            quantity: Units::new(100).unwrap(),
            unit_price: term(Money::parse(price, CUR).unwrap(), id),
        }],
        tooling: not_applicable_term("no tooling"),
        nre: not_applicable_term("no NRE"),
        amortization_units: None,
        payment_days,
        moq: Units::new(moq).unwrap(),
        multiple: Units::new(1).unwrap(),
        lead_time_days: term(
            Distribution {
                p10: 20,
                p50: 28,
                p90: 35,
                p99: 45,
                sample_size: 30,
            },
            id,
        ),
        valid_until: Some(Day(DAY + 90)),
        warranty_days: 365,
        ncnr: false,
        incoterm: Incoterm::Fob,
        mode: TransportMode::Air,
        freight_estimates: vec![],
        document: EvidenceRef::new(id, Revision(2), None).unwrap(),
    }
}

#[allow(clippy::vec_init_then_push)]
fn should_cost_input(inspection_cost: &str) -> ShouldCostInput {
    let money = |amount: &str| Money::parse(amount, CUR).unwrap();
    ShouldCostInput {
        currency: CUR,
        materials: vec![term(
            MaterialLine {
                description: "10 mm bore bar".to_string(),
                quantity_per_unit: ScaledQuantity::new(1_000_000).unwrap(),
                unit_price: money("2.00"),
                yield_rate: Rate::new(950_000).unwrap(),
            },
            "sc-material",
        )],
        process_steps: vec![ProcessStep {
            operation: "turn and bore".to_string(),
            machine: term(
                MachineTime {
                    rate_per_hour: money("60.00"),
                    cycle_seconds: 30,
                },
                "sc-machine",
            ),
            setup: term(
                SetupCost {
                    setup_cost: money("10.00"),
                    lot_size: Units::new(1_000).unwrap(),
                },
                "sc-setup",
            ),
            tooling: not_applicable_term("no tooling"),
            labor: term(
                LaborTime {
                    rate_per_hour: money("30.00"),
                    minutes_per_unit: 1,
                },
                "sc-labor",
            ),
            yield_rate: term(Rate::new(980_000).unwrap(), "sc-yield"),
        }],
        inspection: term(money(inspection_cost), "sc-inspection"),
        scrap_rework: term(
            ScrapRework {
                scrap_rate: Rate::new(20_000).unwrap(),
                rework_rate: Rate::new(10_000).unwrap(),
                rework_cost_per_unit: money("0.50"),
            },
            "sc-scrap",
        ),
        overhead: term(
            Overhead {
                rate: Rate::new(100_000).unwrap(),
                base: OverheadBase::Conversion,
            },
            "sc-overhead",
        ),
        packaging: term(money("0.05"), "sc-packaging"),
        margin: term(Rate::new(100_000).unwrap(), "sc-margin"),
        margin_mode: MarginMode::MarkupOnCost,
    }
}

#[allow(clippy::vec_init_then_push)]
fn capacitor_requirements() -> EngineeringRequirementSet {
    let mut requirements = Vec::new();
    requirements.push(EngineeringRequirement {
        id: "capacitance".to_string(),
        statement: "capacitance is 10 uF +/- 10% at the stated condition".to_string(),
        discipline: Discipline::Electrical,
        mandatory: true,
        target: RequirementTarget::Numeric {
            property: "capacitance".to_string(),
            limit: NumericLimit::EqualWithin {
                nominal: quantity(10.0, "uF"),
                tolerance: quantity(1.0, "uF"),
            },
            model: Some("model-cap".to_string()),
        },
        conditions: vec!["25 C ambient".to_string()],
        provenance: eng_provenance("capacitor requirement"),
    });
    requirements.push(EngineeringRequirement {
        id: "voltage".to_string(),
        statement: "rated voltage is at least 16 V".to_string(),
        discipline: Discipline::Electrical,
        mandatory: true,
        target: RequirementTarget::Numeric {
            property: "voltage".to_string(),
            limit: NumericLimit::Min {
                limit: quantity(16.0, "V"),
            },
            model: Some("model-cap".to_string()),
        },
        conditions: vec![],
        provenance: eng_provenance("capacitor requirement"),
    });
    requirements.push(EngineeringRequirement {
        id: "esr".to_string(),
        statement: "ESR is at most 100 mohm at 100 kHz".to_string(),
        discipline: Discipline::Electrical,
        mandatory: true,
        target: RequirementTarget::Numeric {
            property: "esr".to_string(),
            limit: NumericLimit::Max {
                limit: quantity(100.0, "mohm"),
            },
            model: Some("model-cap".to_string()),
        },
        conditions: vec!["100 kHz".to_string()],
        provenance: eng_provenance("capacitor requirement"),
    });
    requirements.push(EngineeringRequirement {
        id: "ripple".to_string(),
        statement: "rated ripple current is at least 1 A at 105 C".to_string(),
        discipline: Discipline::Thermal,
        mandatory: true,
        target: RequirementTarget::Numeric {
            property: "ripple_current".to_string(),
            limit: NumericLimit::Min {
                limit: quantity(1.0, "A"),
            },
            model: Some("model-cap".to_string()),
        },
        conditions: vec!["105 C".to_string()],
        provenance: eng_provenance("capacitor requirement"),
    });
    requirements.push(EngineeringRequirement {
        id: "case".to_string(),
        statement: "case length is at most 3.2 mm".to_string(),
        discipline: Discipline::Tolerance,
        mandatory: true,
        target: RequirementTarget::Numeric {
            property: "case_length".to_string(),
            limit: NumericLimit::Max {
                limit: quantity(3.2, "mm"),
            },
            model: None,
        },
        conditions: vec![],
        provenance: eng_provenance("capacitor requirement"),
    });
    requirements.push(EngineeringRequirement {
        id: "dielectric".to_string(),
        statement: "dielectric is X7R or X5R".to_string(),
        discipline: Discipline::Electrical,
        mandatory: true,
        target: RequirementTarget::Categorical {
            property: "dielectric".to_string(),
            allowed: vec!["X7R".to_string(), "X5R".to_string()],
            model: Some("model-cap".to_string()),
        },
        conditions: vec![],
        provenance: eng_provenance("capacitor requirement"),
    });
    requirements.push(EngineeringRequirement {
        id: "load_life".to_string(),
        statement: "load-life evidence reaches 1000 h".to_string(),
        discipline: Discipline::Reliability,
        mandatory: false,
        target: RequirementTarget::Numeric {
            property: "load_life_hours".to_string(),
            limit: NumericLimit::Min {
                limit: quantity(1_000.0, "h"),
            },
            model: None,
        },
        conditions: vec![],
        provenance: eng_provenance("capacitor requirement"),
    });
    EngineeringRequirementSet {
        id: "capacitor-set".to_string(),
        revision: revision(),
        objective: "qualify a capacitor alternate without hidden assumptions".to_string(),
        scope: "10 uF 25 V decoupling capacitor at the stated conditions".to_string(),
        requirements,
        constraints: vec![],
        models: vec![ModelBinding {
            model_id: "model-cap".to_string(),
            version: "1.0.0".to_string(),
            equation_ids: vec!["eq-cap-ripple-loss".to_string()],
            temperature: Some(engineering_units::Interval::new(233.15, 398.15).unwrap()),
            conditions: vec![],
            exclusions: vec![],
            pack_id: Some("passives".to_string()),
            pack_version: Some("1.0.0".to_string()),
            provenance: eng_provenance("capacitor model"),
        }],
        evidence: vec![RequirementEvidence {
            id: "capacitor-requirement-rev-7".to_string(),
            revision: RevisionId::new(7),
            digest: None,
            provenance: eng_provenance("capacitor requirement"),
        }],
    }
}

fn capacitor_facts(
    id: &str,
    esr: f64,
    ripple: f64,
    dielectric: &str,
    life: Option<f64>,
) -> CandidateFacts {
    let mut values = BTreeMap::new();
    for (name, value, unit) in [
        ("capacitance", 10.1, "uF"),
        ("voltage", 25.0, "V"),
        ("esr", esr, "mohm"),
        ("ripple_current", ripple, "A"),
        ("case_length", 3.2, "mm"),
    ] {
        values.insert(
            name.to_string(),
            engineering_core::EngineeringValue::known(
                quantity(value, unit),
                eng_provenance(&format!("{id}:{name}")),
                revision(),
            )
            .unwrap(),
        );
    }
    if let Some(life) = life {
        values.insert(
            "load_life_hours".to_string(),
            engineering_core::EngineeringValue::known(
                quantity(life, "h"),
                eng_provenance(&format!("{id}:load_life")),
                revision(),
            )
            .unwrap(),
        );
    }
    let mut categorical = BTreeMap::new();
    categorical.insert(
        "dielectric".to_string(),
        CategoricalFact {
            value: dielectric.to_string(),
            provenance: eng_provenance(&format!("{id}:dielectric")),
            revision: revision(),
        },
    );
    CandidateFacts {
        candidate_id: id.to_string(),
        node: None,
        revision: revision(),
        values,
        categorical,
        conditions: vec![
            "25 C ambient".to_string(),
            "100 kHz".to_string(),
            "105 C".to_string(),
        ],
        temperature: Some(quantity(298.15, "K")),
        evidence: vec![RequirementEvidence {
            id: format!("{id}-datasheet"),
            revision: RevisionId::new(2),
            digest: None,
            provenance: eng_provenance(&format!("{id}:datasheet")),
        }],
    }
}

fn request(
    candidates: Vec<LoopCandidate>,
    engineering: EngineeringRequirementSet,
    relaxations: Vec<Relaxation>,
) -> LoopRequest {
    LoopRequest {
        decision_id: "decision-cap".to_string(),
        revision: Revision(3),
        engineering_revision: engineering.revision,
        requirement: SupplyRequirement {
            id: "req-cap".to_string(),
            part: Some(PartId(1)),
            description: "10 uF 25 V decoupling capacitor".to_string(),
            quantity: Units::new(1_000).unwrap(),
            needed_by: Day(DAY + 30),
            required_processes: vec![],
            required_qualifications: vec![],
            currency: CUR,
        },
        engineering,
        sourcing: None,
        require_supplier_fit: false,
        candidates,
        knowledge_domains: vec!["passives".to_string()],
        assumptions: vec![],
        evidence: vec![EvidenceRef::new("decision-input", Revision(3), None).unwrap()],
        inventory: InventoryContext {
            on_hand: Units::new(100).unwrap(),
            confirmed: Units::new(200).unwrap(),
            safety_stock: Units::new(50).unwrap(),
        },
        negotiation: NegotiationContext {
            cost_of_capital: Rate::new(120_000).unwrap(),
            baseline_payment_days: 30,
            destination_country: "US".to_string(),
            requested_incoterm: Incoterm::Fob,
            transport_mode: TransportMode::Air,
        },
        relaxations,
    }
}

#[allow(clippy::too_many_arguments)]
fn candidate(
    id: &str,
    supplier: u32,
    facts: CandidateFacts,
    unit_price: &str,
    freight: &str,
    moq: u64,
    defect_ppm: i64,
    payment_days: u32,
) -> LoopCandidate {
    LoopCandidate {
        id: CandidateId(id.to_string()),
        supplier: SupplierId(supplier),
        role: SupplierRole::Manufacturer,
        site: Some(SiteId(supplier)),
        part: Some(PartId(1)),
        processes: vec!["smt".to_string()],
        facts,
        supplier_profile: None,
        quote: Some(quote(
            &format!("{id}-quote"),
            supplier,
            unit_price,
            moq,
            payment_days,
        )),
        relaxed_quote: None,
        should_cost: None,
        relaxed_should_cost: None,
        tco: Some(tco_input(
            1_000,
            unit_price,
            freight,
            moq,
            defect_ppm,
            payment_days,
        )),
        relaxed_tco: None,
        lead_time_days: term(payment_days, &format!("{id}-lead")),
        transit_days: Availability::known(5),
        chargeable_weight_g: Availability::known(1_200),
        quality_risk: None,
        continuity_risk: None,
        lifecycle_risk: None,
        evidence: vec![EvidenceRef::new(&format!("{id}-evidence"), Revision(2), None).unwrap()],
    }
}

#[test]
fn capacitor_alternate_is_rejected_on_dielectric_esr_and_ripple() {
    let engineering = capacitor_requirements();
    let baseline = candidate(
        "baseline",
        1,
        capacitor_facts("baseline", 50.0, 1.2, "X7R", Some(2_000.0)),
        "1.00",
        "100.00",
        100,
        100,
        30,
    );
    let y5v = candidate(
        "y5v-cheap",
        2,
        capacitor_facts("y5v-cheap", 220.0, 0.5, "Y5V", Some(1_500.0)),
        "0.70",
        "100.00",
        100,
        100,
        30,
    );
    let qualified = candidate(
        "x5r-qualified",
        3,
        capacitor_facts("x5r-qualified", 80.0, 1.1, "X5R", None),
        "1.10",
        "100.00",
        100,
        100,
        30,
    );
    let outcome = run(&request(
        vec![baseline, y5v, qualified],
        engineering,
        vec![],
    ))
    .unwrap();

    assert!(
        outcome.is_verified(),
        "verification: {:?}",
        outcome.verify()
    );
    assert!(matches!(
        outcome.decision.recommended_action,
        RecommendedAction::Proceed { .. }
    ));
    let y5v = outcome
        .evaluations
        .iter()
        .find(|evaluation| evaluation.candidate.0 == "y5v-cheap")
        .unwrap();
    assert_eq!(y5v.engineering.verdict, EngineeringVerdict::Reject);
    assert!(y5v
        .derived_tests
        .iter()
        .any(|test| test.knowledge_ref.as_deref() == Some("passives@1.0.0#test-cap-esr")));
    assert!(y5v
        .derived_tests
        .iter()
        .any(|test| test.knowledge_ref.as_deref() == Some("passives@1.0.0#test-cap-ripple-life")));
    let qualified = outcome
        .evaluations
        .iter()
        .find(|evaluation| evaluation.candidate.0 == "x5r-qualified")
        .unwrap();
    assert_eq!(qualified.engineering.verdict, EngineeringVerdict::Qualify);
    assert!(!qualified.derived_tests.is_empty());
    let response = outcome.response.as_ref().unwrap();
    assert!(response.is_ready(), "gates: {:?}", response.assess());
    let inventory = outcome.decision.inventory_effect.as_known().unwrap();
    assert_eq!(inventory.net_requirement.get(), 700);
    assert_eq!(inventory.safety_stock.get(), 50);
    assert!(outcome.decision.logistics_plan.is_known());
    assert!(outcome
        .decision
        .evidence
        .iter()
        .any(|evidence| evidence.id == "capacitor-requirement-rev-7"));
}

#[test]
fn a_stored_result_survives_json_roundtrip_and_verifies() {
    let engineering = capacitor_requirements();
    let candidate = candidate(
        "baseline",
        1,
        capacitor_facts("baseline", 50.0, 1.2, "X7R", Some(2_000.0)),
        "1.00",
        "100.00",
        100,
        100,
        30,
    );
    let outcome = run(&request(vec![candidate], engineering, vec![])).unwrap();
    let encoded = serde_json::to_string(&outcome).unwrap();
    let decoded: LoopOutcome = serde_json::from_str(&encoded).unwrap();
    let verification = decoded.verify().unwrap();
    assert!(
        decoded.is_verified(),
        "roundtrip verification: {verification:?}"
    );
}

#[test]
fn required_supplier_qualification_blocks_an_unassessed_candidate() {
    let engineering = capacitor_requirements();
    let candidate = candidate(
        "unassessed",
        1,
        capacitor_facts("unassessed", 50.0, 1.2, "X7R", Some(2_000.0)),
        "1.00",
        "100.00",
        100,
        100,
        30,
    );
    let mut request = request(vec![candidate], engineering, vec![]);
    request.require_supplier_fit = true;
    let outcome = run(&request).unwrap();
    assert!(matches!(
        outcome.decision.recommended_action,
        RecommendedAction::RequestEvidence { .. }
    ));
    assert!(outcome.evaluations[0]
        .blockers
        .iter()
        .any(|blocker| blocker.contains("supplier fit not assessed")));
    assert!(outcome.evaluations[0].supplier_fit.is_none());
}

#[test]
fn tolerance_relaxation_updates_engineering_revision_and_supply_terms() {
    let mut engineering = capacitor_requirements();
    engineering.id = "tolerance-set".to_string();
    engineering.objective = "meet the hole tolerance with evidenced process capability".to_string();
    engineering.requirements = vec![EngineeringRequirement {
        id: "hole".to_string(),
        statement: "hole diameter is 10 mm +/- 0.010 mm".to_string(),
        discipline: Discipline::Tolerance,
        mandatory: true,
        target: RequirementTarget::Numeric {
            property: "hole_diameter".to_string(),
            limit: NumericLimit::EqualWithin {
                nominal: quantity(10.0, "mm"),
                tolerance: quantity(0.010, "mm"),
            },
            model: Some("model-process".to_string()),
        },
        conditions: vec!["process capability study".to_string()],
        provenance: eng_provenance("tolerance requirement"),
    }];
    engineering.models = vec![ModelBinding {
        model_id: "model-process".to_string(),
        version: "1.0.0".to_string(),
        equation_ids: vec!["eq-process-capability".to_string()],
        temperature: None,
        conditions: vec!["process capability study".to_string()],
        exclusions: vec![],
        pack_id: None,
        pack_version: None,
        provenance: eng_provenance("process capability model"),
    }];
    engineering.evidence = vec![RequirementEvidence {
        id: "tolerance-requirement-rev-7".to_string(),
        revision: RevisionId::new(7),
        digest: None,
        provenance: eng_provenance("tolerance requirement"),
    }];

    let mut facts = capacitor_facts("process", 50.0, 1.2, "X7R", Some(2_000.0));
    facts.values.clear();
    facts.categorical.clear();
    facts.conditions = vec!["process capability study".to_string()];
    facts.values.insert(
        "hole_diameter".to_string(),
        engineering_core::EngineeringValue::known(
            quantity(10.018, "mm"),
            eng_provenance("process:hole"),
            revision(),
        )
        .unwrap(),
    );

    let mut candidate = candidate("process", 1, facts, "5.00", "100.00", 100, 100, 30);
    candidate.quote = Some(quote("process-quote", 1, "5.00", 100, 30));
    candidate.should_cost = Some(should_cost_input("1.00"));
    candidate.relaxed_quote = Some(quote("process-relaxed-quote", 1, "4.00", 100, 30));
    candidate.relaxed_tco = Some(tco_input(1_000, "4.00", "100.00", 100, 100, 30));
    candidate.relaxed_should_cost = Some(should_cost_input("0.20"));

    let relaxation = Relaxation {
        requirement_id: "hole".to_string(),
        new_limit: NumericLimit::EqualWithin {
            nominal: quantity(10.0, "mm"),
            tolerance: quantity(0.025, "mm"),
        },
        rationale: "the measured process capability supports a wider tolerance".to_string(),
        provenance: eng_provenance("capability study"),
        falsifier: "a capability study that still shows more than 0.025 mm".to_string(),
        evidence: RequirementEvidence {
            id: "cpk-study".to_string(),
            revision: RevisionId::new(2),
            digest: None,
            provenance: eng_provenance("capability study"),
        },
    };

    let outcome = run(&request(vec![candidate], engineering, vec![relaxation])).unwrap();
    assert!(
        outcome.is_verified(),
        "verification: {:?}",
        outcome.verify()
    );
    assert_eq!(outcome.applied_relaxations.len(), 1);
    assert_eq!(outcome.requirement_set.revision, RevisionId::new(8));
    assert_eq!(
        outcome.evaluations[0].engineering.verdict,
        EngineeringVerdict::Accept
    );
    assert!(matches!(
        outcome.decision.recommended_action,
        RecommendedAction::Proceed { .. }
    ));
    let terms = outcome.decision.commercial_terms.as_known().unwrap();
    assert_eq!(
        terms.unit_price.as_known().unwrap().micros(),
        4_000_000,
        "the relaxed RFQ terms are the ones used"
    );
    let relaxed_should_cost = should_cost(&should_cost_input("0.20")).unwrap();
    assert_eq!(
        outcome.evaluations[0]
            .should_cost
            .as_ref()
            .unwrap()
            .ex_works,
        relaxed_should_cost.ex_works,
        "the relaxed should-cost basis is the one used"
    );
    assert!(outcome.negotiation.is_some());
    assert!(outcome
        .decision
        .counterfactuals
        .iter()
        .any(|counterfactual| {
            counterfactual.description.contains("pre-relaxation")
                && counterfactual.outcome.contains("reject")
        }));
}

#[test]
fn cheaper_unit_price_loses_on_moq_freight_quality_and_cash() {
    let engineering = capacitor_requirements();
    let incumbent = candidate(
        "incumbent",
        1,
        capacitor_facts("incumbent", 50.0, 1.2, "X7R", Some(2_000.0)),
        "1.00",
        "100.00",
        100,
        100,
        30,
    );
    let cheap = candidate(
        "cheap-unit-price",
        2,
        capacitor_facts("cheap-unit-price", 50.0, 1.2, "X7R", Some(2_000.0)),
        "0.90",
        "5000.00",
        5_000,
        50_000,
        90,
    );
    let outcome = run(&request(vec![incumbent, cheap], engineering, vec![])).unwrap();

    assert!(
        outcome.is_verified(),
        "verification: {:?}",
        outcome.verify()
    );
    assert_eq!(outcome.ranked[0].0, "incumbent");
    let recommended_total = outcome
        .decision
        .landed_cost
        .as_known()
        .unwrap()
        .total
        .as_known()
        .unwrap();
    let cheap_evaluation = outcome
        .evaluations
        .iter()
        .find(|evaluation| evaluation.candidate.0 == "cheap-unit-price")
        .unwrap();
    assert!(
        cheap_evaluation.landed_total.as_known().unwrap().micros() > recommended_total.micros(),
        "the cheaper unit price must lose on landed TCO"
    );
    assert!(outcome
        .decision
        .counterfactuals
        .iter()
        .any(|counterfactual| {
            counterfactual
                .description
                .contains("lowest quoted unit price")
                && counterfactual.outcome.contains("does not survive")
        }));
}

#[test]
fn authorization_gates_remain_on_orders_and_messages() {
    let request = ActionRequest {
        action: ActionKind::CreatePurchaseOrder,
        scope: "buy 1000 capacitors".to_string(),
        counterparty: Some(SupplierId(1)),
        amount: Some(Money::parse("1000.00", CUR).unwrap()),
        as_of: Day(DAY),
    };
    let authorization = authorize(&request, &[] as &[AuthorizationGrant]);
    assert!(matches!(
        authorization,
        faktor_supply_execution::AuthorizationDecision::Denied { .. }
    ));
}

fn perf_candidates(count: u32) -> Vec<LoopCandidate> {
    let mut candidates = Vec::with_capacity(count as usize);
    for index in 0..count {
        let mut facts = capacitor_facts(
            &format!("candidate-{index:04}"),
            50.0,
            1.2,
            "X7R",
            Some(2_000.0),
        );
        facts.candidate_id = format!("candidate-{index:04}");
        let mut candidate = candidate(
            &format!("candidate-{index:04}"),
            index + 1,
            facts,
            "1.00",
            "100.00",
            100,
            100,
            30,
        );
        let price = Money::from_micros(1_000_000 + i64::from(index), CUR).unwrap();
        candidate.tco.as_mut().unwrap().ex_works_unit =
            term(price, &format!("candidate-{index:04}-price"));
        candidate.quote = None;
        candidate.transit_days = Availability::known(5);
        candidate.lead_time_days = term(30, &format!("candidate-{index:04}-lead"));
        candidates.push(candidate);
    }
    candidates
}

#[test]
fn one_thousand_candidates_evaluate_and_rank_with_measured_work() {
    let engineering = capacitor_requirements();
    let candidates = perf_candidates(500);
    let started_half = Instant::now();
    let half = run(&request(candidates, engineering.clone(), vec![])).unwrap();
    let elapsed_half = started_half.elapsed();
    assert_eq!(half.ranked.len(), 500);

    let candidates = perf_candidates(1_000);
    let started = Instant::now();
    let outcome = run(&request(candidates, engineering, vec![])).unwrap();
    let elapsed = started.elapsed();
    assert!(
        outcome.is_verified(),
        "verification: {:?}",
        outcome.verify()
    );
    assert_eq!(outcome.performance.evaluated_candidates, 1_000);
    assert_eq!(outcome.performance.promotable_candidates, 1_000);
    assert!(outcome.evaluations.len() <= MAX_LOOP_RANKED);
    assert_eq!(outcome.ranked.len(), 1_000);
    assert_eq!(outcome.ranked[0].0, "candidate-0000");
    let ratio = elapsed.as_secs_f64() / elapsed_half.as_secs_f64().max(f64::EPSILON);
    println!(
        "500/1k candidate loop: {:?}/{:?} (kernel {} us, returned {}), ratio {:.2}",
        elapsed_half,
        elapsed,
        outcome.performance.elapsed_micros,
        outcome.performance.returned_candidates,
        ratio
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "1k candidate loop exceeded the bound: {elapsed:?}"
    );
    assert!(
        ratio < 3.5,
        "1k/500 ratio {ratio:.2} suggests worse than n log n"
    );
}

#[test]
fn malformed_candidate_fact_units_are_refused_not_coerced() {
    let engineering = capacitor_requirements();
    let mut facts = capacitor_facts("bad-units", 50.0, 1.2, "X7R", Some(2_000.0));
    facts.values.insert(
        "esr".to_string(),
        engineering_core::EngineeringValue::known(
            quantity(50.0, "V"),
            eng_provenance("bad-units:esr"),
            revision(),
        )
        .unwrap(),
    );
    let candidate = candidate("bad-units", 1, facts, "1.00", "100.00", 100, 100, 30);
    let outcome = run(&request(vec![candidate], engineering, vec![])).unwrap();
    let evaluation = &outcome.evaluations[0];
    assert_eq!(evaluation.engineering.verdict, EngineeringVerdict::Reject);
    assert!(matches!(
        evaluation.engineering.outcomes[2].state,
        OutcomeState::DimensionMismatch { .. }
    ));
}

#[test]
fn unknown_conditions_stay_unavailable_and_do_not_fail_the_crosscheck() {
    let engineering = capacitor_requirements();
    let mut facts = capacitor_facts("missing-condition", 50.0, 1.2, "X7R", Some(2_000.0));
    facts.conditions.retain(|condition| condition != "105 C");
    let candidate = candidate(
        "missing-condition",
        1,
        facts,
        "1.00",
        "100.00",
        100,
        100,
        30,
    );
    let outcome = run(&request(vec![candidate], engineering, vec![])).unwrap();
    let evaluation = &outcome.evaluations[0];
    assert_eq!(
        evaluation.engineering.verdict,
        EngineeringVerdict::Unavailable
    );
    assert!(evaluation
        .engineering
        .outcomes
        .iter()
        .any(|outcome| matches!(outcome.state, OutcomeState::Unavailable { .. })));
    assert!(matches!(
        outcome.decision.recommended_action,
        RecommendedAction::RequestEvidence { .. }
    ));
    assert_eq!(
        outcome.receipt.input_digest,
        outcome.cross_check.input_digest
    );
}

#[test]
fn fixture_supply_term_types_are_reachable() {
    let _: faktor_supply_core::Term<u32> = term(1, "x");
    let _: InventorySummary = InventorySummary {
        on_hand: Units::ZERO,
        confirmed: Units::ZERO,
        net_requirement: Units::ZERO,
        safety_stock: Units::ZERO,
        evidence: vec![],
    };
    let _ = ExactError::Overflow;
}
