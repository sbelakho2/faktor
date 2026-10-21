use std::path::PathBuf;

use engineering_knowledge::{KnowledgeError, KnowledgePack, KnowledgeRegistry, PACK_SCHEMA};
use serde_json::{json, Value};

fn bundle(name: &str) -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("packs")
            .join(name),
    )
    .unwrap()
}

fn mutate(bytes: &[u8], change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(bytes).unwrap();
    change(&mut value);
    serde_json::to_vec(&value).unwrap()
}

#[test]
fn starter_packs_are_file_loaded_and_digest_stable() {
    let registry = KnowledgeRegistry::bundled().unwrap();
    assert_eq!(registry.len(), 10);
    for (file, id, domain) in [
        ("bolted_joint.json", "bolted-joint", "machine_design"),
        (
            "power_electronics.json",
            "power-electronics",
            "power_electronics",
        ),
        (
            "injection_molding.json",
            "injection-molding",
            "manufacturing",
        ),
        ("analog_stability.json", "analog-stability", "analog"),
        (
            "flyback_converters.json",
            "flyback-converters",
            "power_electronics",
        ),
        ("thermal_networks.json", "thermal-networks", "thermal"),
        ("fatigue_materials.json", "fatigue-materials", "fatigue"),
        ("emc_pcb.json", "emc-pcb", "emc"),
        (
            "materials_conditional.json",
            "materials-conditional",
            "materials",
        ),
    ] {
        let bytes = bundle(file);
        let pack = KnowledgePack::load(&bytes).unwrap();
        assert_eq!(pack.id, id);
        assert_eq!(pack.domain, domain);
        assert_eq!(pack.schema, PACK_SCHEMA);
        assert_eq!(
            pack.digest(),
            Some(engineering_core::Digest::of_bytes(&bytes))
        );
        assert_eq!(registry.get(id, &pack.version).unwrap().id, id);
        let authority = pack.authority().unwrap();
        assert_eq!(authority.pack_id, id);
        assert_eq!(authority.digest, engineering_core::Digest::of_bytes(&bytes));
        assert!(!authority.scope.is_empty());
    }
}

#[test]
fn every_pack_item_carries_provenance_and_a_nonempty_category() {
    for pack in KnowledgeRegistry::bundled().unwrap().packs() {
        assert!(!pack.terminology.is_empty());
        assert!(!pack.models.is_empty());
        assert!(!pack.equations.is_empty());
        assert!(!pack.assumptions.is_empty());
        assert!(!pack.validity_domains.is_empty());
        assert!(!pack.property_sources.is_empty());
        assert!(!pack.design_rules.is_empty());
        assert!(!pack.manufacturing_rules.is_empty());
        assert!(!pack.failure_modes.is_empty());
        assert!(!pack.diagnostic_signatures.is_empty());
        assert!(!pack.test_methods.is_empty());
        assert!(!pack.standards_mappings.is_empty());
        assert!(!pack.worked_cases.is_empty());
        assert!(!pack.counterexamples.is_empty());
        assert!(!pack.known_traps.is_empty());
        for provenance in pack
            .terminology
            .iter()
            .map(|item| &item.provenance)
            .chain(pack.models.iter().map(|item| &item.provenance))
            .chain(pack.equations.iter().map(|item| &item.provenance))
            .chain(pack.assumptions.iter().map(|item| &item.provenance))
            .chain(pack.validity_domains.iter().map(|item| &item.provenance))
            .chain(pack.property_sources.iter().map(|item| &item.provenance))
            .chain(pack.design_rules.iter().map(|item| &item.provenance))
            .chain(pack.manufacturing_rules.iter().map(|item| &item.provenance))
            .chain(pack.failure_modes.iter().map(|item| &item.provenance))
            .chain(
                pack.diagnostic_signatures
                    .iter()
                    .map(|item| &item.provenance),
            )
            .chain(pack.test_methods.iter().map(|item| &item.provenance))
            .chain(pack.standards_mappings.iter().map(|item| &item.provenance))
            .chain(pack.worked_cases.iter().map(|item| &item.provenance))
            .chain(pack.counterexamples.iter().map(|item| &item.provenance))
            .chain(pack.known_traps.iter().map(|item| &item.provenance))
        {
            assert!(!provenance.citation.trim().is_empty());
            assert!(!provenance.kind.trim().is_empty());
        }
    }
}

#[test]
fn missing_provenance_is_refused() {
    let bytes = mutate(&bundle("bolted_joint.json"), |value| {
        value["terminology"][0]["provenance"]["citation"] = json!("   ");
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::EmptyField { .. })
    ));
}

#[test]
fn dangling_model_references_are_refused() {
    let bytes = mutate(&bundle("bolted_joint.json"), |value| {
        value["models"][0]["equation_ids"] = json!(["eq-does-not-exist"]);
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::MissingReference { .. })
    ));
}

#[test]
fn duplicate_ids_inside_a_pack_are_refused() {
    let bytes = mutate(&bundle("bolted_joint.json"), |value| {
        value["terminology"][1]["id"] = json!("term-preload");
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::DuplicateId { .. })
    ));
}

#[test]
fn an_empty_category_is_refused() {
    let bytes = mutate(&bundle("injection_molding.json"), |value| {
        value["design_rules"] = json!([]);
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::EmptyField { .. })
    ));
}

#[test]
fn unknown_fields_and_schema_versions_are_refused() {
    let bytes = mutate(&bundle("bolted_joint.json"), |value| {
        value["generated_facts"] = json!(["invented"]);
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::Json(_))
    ));

    let bytes = mutate(&bundle("bolted_joint.json"), |value| {
        value["schema"] = json!("faktor.engineering.knowledge-pack/v0");
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::SchemaMismatch { .. })
    ));
}

#[test]
fn nonsemantic_versions_are_refused() {
    let bytes = mutate(&bundle("bolted_joint.json"), |value| {
        value["version"] = json!("1.0");
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::InvalidVersion { .. })
    ));
}

#[test]
fn standards_must_remain_metadata_only() {
    let bytes = mutate(&bundle("bolted_joint.json"), |value| {
        value["standards_mappings"][0]["access"] = json!("embedded_text");
    });
    assert!(matches!(
        KnowledgePack::load(&bytes),
        Err(KnowledgeError::Json(_))
    ));
}

#[test]
fn a_duplicate_pack_version_is_refused_but_a_new_version_is_accepted() {
    let mut registry = KnowledgeRegistry::new(8).unwrap();
    let bytes = bundle("bolted_joint.json");
    registry.register_json(&bytes).unwrap();
    assert!(matches!(
        registry.register_json(&bytes),
        Err(KnowledgeError::DuplicatePack { .. })
    ));
    let next = mutate(&bytes, |value| {
        value["version"] = json!("1.0.1");
    });
    registry.register_json(&next).unwrap();
    assert_eq!(registry.latest("bolted-joint").unwrap().version, "1.0.1");
}

#[test]
fn pack_registry_limit_is_bounded() {
    let mut registry = KnowledgeRegistry::new(1).unwrap();
    registry
        .register_json(&bundle("bolted_joint.json"))
        .unwrap();
    assert!(matches!(
        registry.register_json(&bundle("power_electronics.json")),
        Err(KnowledgeError::PackLimit { .. })
    ));
}
