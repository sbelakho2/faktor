use engineering_core::{
    CatalogModel, Discipline, ModelCatalog, ModelSelector, RequirementSelection,
    SelectionProvenance, SelectionRequest, ValidityDomain,
};
use engineering_knowledge::KnowledgeRegistry;
use engineering_units::{Interval, Quantity, UnitRegistry};

fn kelvin(value: f64) -> Quantity {
    Quantity::new(value, UnitRegistry::si().require("K").unwrap()).unwrap()
}

fn request(
    id: &str,
    discipline: Discipline,
    property: &str,
    conditions: &[&str],
    temperature: Option<f64>,
) -> SelectionRequest {
    SelectionRequest {
        requirement_id: id.to_string(),
        discipline,
        property: Some(property.to_string()),
        conditions: conditions
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        material: None,
        temperature: temperature.map(kelvin),
    }
}

fn select(request: &SelectionRequest) -> engineering_core::ApplicabilitySelection {
    let catalog = KnowledgeRegistry::bundled()
        .unwrap()
        .model_catalog()
        .unwrap();
    ModelSelector::select(&catalog, std::slice::from_ref(request)).unwrap()
}

#[test]
fn every_shipped_pack_selects_its_registered_model_in_domain() {
    let cases = [
        (
            request("bolt", Discipline::Mechanical, "preload", &[], None),
            "model-torque-preload",
        ),
        (
            request(
                "mosfet",
                Discipline::Electrical,
                "junction_temperature",
                &[],
                Some(330.0),
            ),
            "model-conduction-thermal",
        ),
        (
            request("mold", Discipline::Manufacturing, "draft_angle", &[], None),
            "model-moldability-checks",
        ),
        (
            request("cap", Discipline::Electrical, "esr", &[], Some(298.15)),
            "model-cap-thermal-ripple",
        ),
        (
            request("opamp", Discipline::Electrical, "phase_margin", &[], None),
            "model-opamp-loop-stability",
        ),
        (
            request("flyback", Discipline::Electrical, "vds_peak", &[], None),
            "model-flyback-converter",
        ),
        (
            request(
                "thermal-network",
                Discipline::Thermal,
                "rc_ladder",
                &[],
                Some(330.0),
            ),
            "model-thermal-network",
        ),
        (
            request("fatigue", Discipline::Reliability, "damage", &[], None),
            "model-fatigue-damage",
        ),
        (
            request("emc", Discipline::Emc, "loop_inductance", &[], None),
            "model-pcb-loop-inductance",
        ),
        (
            request(
                "material-property",
                Discipline::Mechanical,
                "yield_strength",
                &[],
                Some(300.0),
            ),
            "model-material-property-lookup",
        ),
    ];
    for (request, expected) in cases {
        let selection = select(&request);
        assert!(selection.is_all_selected(), "{selection:?}");
        let model = selection.unique_models()[0];
        assert_eq!(model.model_id, expected);
        assert!(!model.rationale.is_empty());
        assert!(!model.validity_domain_ids.is_empty());
        assert!(model.pack_digest.is_some());
        assert!(model.provenance.citation.len() > 4);
        selection.validate().unwrap();
        assert_eq!(selection.envelopes().unwrap().len(), 1);
    }
}

#[test]
fn out_of_domain_temperature_is_typed_research_not_a_selection() {
    let selection = select(&request(
        "cap-hot",
        Discipline::Electrical,
        "esr",
        &[],
        Some(500.0),
    ));
    assert!(!selection.is_all_selected());
    match &selection.selections[0] {
        RequirementSelection::ResearchRequired {
            reason,
            sources_required,
            ..
        } => {
            assert!(reason.contains("outside its validity domain"), "{reason}");
            assert!(!sources_required.is_empty());
        }
        other => panic!("expected research-required, got {other:?}"),
    }
}

#[test]
fn a_registered_exclusion_forces_research() {
    let selection = select(&request(
        "bolt-yield",
        Discipline::Mechanical,
        "preload",
        &["gross yielding"],
        None,
    ));
    assert!(!selection.is_all_selected());
    assert!(matches!(
        selection.selections[0],
        RequirementSelection::ResearchRequired { .. }
    ));
}

#[test]
fn two_applicable_models_are_a_typed_ambiguity() {
    let provenance = SelectionProvenance::new("reference", "ambiguity fixture").unwrap();
    let domain = ValidityDomain {
        id: "domain-any".to_string(),
        statement: "fixture domain".to_string(),
        temperature: None,
        conditions: vec![],
        exclusions: vec![],
        materials: vec![],
        provenance: provenance.clone(),
    };
    let model = |id: &str| CatalogModel {
        model_id: id.to_string(),
        source_model_id: None,
        version: "1.0.0".to_string(),
        pack_id: Some("fixture".to_string()),
        pack_version: Some("1.0.0".to_string()),
        pack_digest: Some(engineering_core::Digest::of_text("fixture")),
        equation_ids: vec!["eq-fixture".to_string()],
        disciplines: vec![Discipline::Thermal],
        properties: vec!["junction".to_string()],
        domains: vec![domain.clone()],
        provenance: provenance.clone(),
    };
    let catalog = ModelCatalog::new(vec![model("model-a"), model("model-b")]).unwrap();
    let selection = ModelSelector::select(
        &catalog,
        &[request(
            "junction",
            Discipline::Thermal,
            "junction_temperature",
            &[],
            Some(300.0),
        )],
    )
    .unwrap();
    assert!(matches!(
        selection.selections[0],
        RequirementSelection::Ambiguous { .. }
    ));
    selection.validate().unwrap();
}

#[test]
fn caller_edits_to_the_selection_are_refused() {
    let selection = select(&request(
        "bolt",
        Discipline::Mechanical,
        "preload",
        &[],
        None,
    ));
    let mut tampered = selection.clone();
    let RequirementSelection::Selected { models, .. } = &mut tampered.selections[0] else {
        panic!("expected a selection");
    };
    models[0].validity[0].temperature = Some(Interval::new(1.0, 10_000.0).unwrap());
    assert!(tampered.validate().is_err());

    let mut forged = selection;
    let RequirementSelection::Selected { models, .. } = &mut forged.selections[0] else {
        panic!("expected a selection");
    };
    models[0].model_id = "caller-preferred".to_string();
    assert!(forged.validate().is_err());
}

#[test]
fn registered_exclusions_force_typed_research_not_a_caller_override() {
    // Breaks if exclusions are recorded but not enforced, or if a caller
    // condition silently replaces the registered domain.
    let selection = select(&request(
        "bolt-excluded",
        Discipline::Mechanical,
        "preload",
        &["uncalibrated torque tools", "gross yielding"],
        None,
    ));
    assert!(!selection.is_all_selected());
    match &selection.selections[0] {
        RequirementSelection::ResearchRequired {
            reason,
            sources_required,
            ..
        } => {
            assert!(reason.contains("excluded"), "{reason}");
            assert!(!sources_required.is_empty());
        }
        other => panic!("expected research-required, got {other:?}"),
    }
}
