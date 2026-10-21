use engineering_core::{
    CatalogModel, Digest, Discipline, EngineeringRequirement, EngineeringRequirementSet,
    ModelBinding, ModelCatalog, ModelSelector, ModelValidityOverride, NumericLimit, Provenance,
    ProvenanceKind, RequirementTarget, RevisionId, SelectionProvenance, SelectionRequest,
    ValidityDomain,
};
use engineering_units::{Quantity, UnitRegistry};
use faktor_approval::{ApprovalKind, ApprovalToken};

fn provenance() -> Provenance {
    Provenance::new(ProvenanceKind::Specification, "authority binding fixture")
        .unwrap()
        .with_revision(RevisionId::new(1))
}

fn requirements(id: &str, revision: u64) -> EngineeringRequirementSet {
    let revision_id = RevisionId::new(revision);
    EngineeringRequirementSet {
        id: id.to_string(),
        revision: revision_id,
        objective: "check a bolted joint".to_string(),
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

fn catalog(version: &str) -> ModelCatalog {
    ModelCatalog::new(vec![CatalogModel {
        model_id: "torque-preload".to_string(),
        source_model_id: None,
        version: version.to_string(),
        pack_id: None,
        pack_version: None,
        pack_digest: None,
        equation_ids: vec!["eq-torque-preload".to_string()],
        disciplines: vec![Discipline::Mechanical],
        properties: vec!["preload".to_string()],
        domains: vec![ValidityDomain {
            id: "val-qualified-nut-factor".to_string(),
            statement: "only the qualified nut-factor regime is covered".to_string(),
            temperature: None,
            conditions: vec!["qualified nut factor".to_string()],
            exclusions: vec![],
            materials: vec![],
            provenance: SelectionProvenance::new("reference", "fixture").unwrap(),
        }],
        provenance: SelectionProvenance::new("reference", "fixture").unwrap(),
    }])
    .unwrap()
}

fn request() -> SelectionRequest {
    SelectionRequest {
        requirement_id: "req-preload".to_string(),
        discipline: Discipline::Mechanical,
        property: Some("preload".to_string()),
        conditions: vec![],
        material: None,
        temperature: None,
    }
}

fn override_for(set: &EngineeringRequirementSet) -> ModelValidityOverride {
    ModelValidityOverride {
        requirement_set_id: set.id.clone(),
        requirement_set_revision: set.revision,
        requirement_set_digest: Digest::of_json(set).unwrap(),
        requirement_id: "req-preload".to_string(),
        model_id: "torque-preload".to_string(),
        rationale: "measured nut factor supports the registered model".to_string(),
    }
}

#[test]
fn model_override_is_bound_to_the_live_set_and_catalog() {
    let set = requirements("set-a", 1);
    let cat = catalog("1.0.0");
    let override_ = override_for(&set);
    let digest = override_.digest(&set, &cat).unwrap();
    let token = ApprovalToken::test_token(
        ApprovalKind::EngineeringModelValidityOverride,
        set.id.clone(),
        set.revision.get().to_string(),
        digest.to_hex(),
    );
    let selection =
        ModelSelector::select_with_override(&cat, &[request()], &set, Some((&override_, &token)))
            .unwrap();
    assert!(selection.is_all_selected());

    // A different requirement set cannot replay the approval.
    let other = requirements("set-b", 2);
    let error =
        ModelSelector::select_with_override(&cat, &[request()], &other, Some((&override_, &token)))
            .unwrap_err();
    assert!(
        matches!(error, engineering_core::CoreError::InvalidSelection { .. }),
        "{error}"
    );

    // A changed catalog (different model definition) cannot replay it either.
    let changed_catalog = catalog("2.0.0");
    let error = ModelSelector::select_with_override(
        &changed_catalog,
        &[request()],
        &set,
        Some((&override_, &token)),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            engineering_core::CoreError::Approval(faktor_approval::ApprovalError::Stale { .. })
        ),
        "{error}"
    );
}

#[test]
fn model_override_without_a_human_token_is_refused() {
    let set = requirements("set-a", 1);
    let cat = catalog("1.0.0");
    let normal = ModelSelector::select(&cat, &[request()]).unwrap();
    // The normal selector refuses to improvise outside the validity domain.
    assert!(!normal.is_all_selected());

    let override_ = override_for(&set);
    let digest = override_.digest(&set, &cat).unwrap();
    let wrong = ApprovalToken::test_token(
        ApprovalKind::EngineeringModelValidityOverride,
        set.id.clone(),
        set.revision.get().to_string(),
        "0".repeat(64),
    );
    let error =
        ModelSelector::select_with_override(&cat, &[request()], &set, Some((&override_, &wrong)))
            .unwrap_err();
    assert!(
        matches!(
            error,
            engineering_core::CoreError::Approval(faktor_approval::ApprovalError::Stale { .. })
        ),
        "a token for a different digest must not override: {error}"
    );
    assert_ne!(digest.to_hex(), "0".repeat(64));
}
