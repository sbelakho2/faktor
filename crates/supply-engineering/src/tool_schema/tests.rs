use std::collections::BTreeMap;

use engineering_core::{
    CandidateFacts, Discipline, EngineeringRequirement, EngineeringRequirementSet, ModelBinding,
    NumericLimit, Provenance as EngineeringProvenance, ProvenanceKind, RequirementTarget,
    RevisionId,
};
use engineering_units::{Quantity, UnitRegistry};
use faktor_supply_core::{
    Availability, CandidateId, Currency, Day, EvidenceRef, Incoterm, PartId, Rate, Revision,
    SupplierId, SupplierRole, SupplyRequirement, TransportMode, UnavailableReason, Units,
};
use serde_json::{json, Value};

use crate::{
    tool_schema::{loop_outcome_schema, loop_request_schema},
    InventoryContext, LoopCandidate, LoopRequest, NegotiationContext,
};

const DAY: i64 = 20_000;

fn unit(symbol: &str) -> engineering_units::Unit {
    UnitRegistry::si().require(symbol).unwrap()
}

fn quantity(value: f64, symbol: &str) -> Quantity {
    Quantity::new(value, unit(symbol)).unwrap()
}

fn engineering_provenance() -> EngineeringProvenance {
    EngineeringProvenance::new(ProvenanceKind::Specification, "datasheet")
        .unwrap()
        .with_revision(RevisionId::new(1))
}

fn example_request() -> LoopRequest {
    let engineering = EngineeringRequirementSet {
        id: "set-1".to_string(),
        revision: RevisionId::new(1),
        objective: "select a decoupling capacitor".to_string(),
        scope: "one 10 uF position".to_string(),
        requirements: vec![EngineeringRequirement {
            id: "cap-min".to_string(),
            statement: "capacitance is at least 1 uF".to_string(),
            discipline: Discipline::Electrical,
            mandatory: true,
            target: RequirementTarget::Numeric {
                property: "capacitance".to_string(),
                limit: NumericLimit::Min {
                    limit: quantity(1.0, "uF"),
                },
                model: Some("cap-model".to_string()),
            },
            conditions: vec![],
            provenance: engineering_provenance(),
        }],
        constraints: vec![],
        models: vec![ModelBinding {
            model_id: "cap-model".to_string(),
            version: "1.0.0".to_string(),
            equation_ids: vec!["eq-capacitance".to_string()],
            temperature: None,
            conditions: vec![],
            exclusions: vec![],
            pack_id: None,
            pack_version: None,
            provenance: engineering_provenance(),
        }],
        evidence: vec![],
    };
    LoopRequest {
        decision_id: "decision-1".to_string(),
        revision: Revision(1),
        engineering_revision: RevisionId::new(1),
        requirement: SupplyRequirement {
            id: "req-1".to_string(),
            part: Some(PartId(1)),
            description: "10 uF 25 V decoupling capacitor".to_string(),
            quantity: Units::new(1_000).unwrap(),
            needed_by: Day(DAY + 30),
            required_processes: vec![],
            required_qualifications: vec![],
            currency: Currency::USD,
        },
        engineering,
        sourcing: None,
        require_supplier_fit: false,
        candidates: vec![LoopCandidate {
            id: CandidateId("candidate-1".to_string()),
            supplier: SupplierId(1),
            role: SupplierRole::Manufacturer,
            site: None,
            part: Some(PartId(1)),
            processes: vec![],
            facts: CandidateFacts {
                candidate_id: "candidate-1".to_string(),
                node: None,
                revision: RevisionId::new(1),
                values: BTreeMap::new(),
                categorical: BTreeMap::new(),
                conditions: vec![],
                temperature: None,
                evidence: vec![],
            },
            supplier_profile: None,
            quote: None,
            relaxed_quote: None,
            should_cost: None,
            relaxed_should_cost: None,
            tco: None,
            relaxed_tco: None,
            lead_time_days: faktor_supply_core::Term::unknown(
                UnavailableReason::MissingEvidence,
                "no lead-time observation",
            ),
            transit_days: Availability::unavailable(
                UnavailableReason::MissingEvidence,
                "no transit observation",
            ),
            chargeable_weight_g: Availability::unavailable(
                UnavailableReason::MissingEvidence,
                "no weight observation",
            ),
            quality_risk: None,
            continuity_risk: None,
            lifecycle_risk: None,
            evidence: vec![EvidenceRef::new("candidate-1-evidence", Revision(1), None).unwrap()],
        }],
        knowledge_domains: vec!["passives".to_string()],
        assumptions: vec![],
        evidence: vec![EvidenceRef::new("decision-input", Revision(1), None).unwrap()],
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
        relaxations: vec![],
        should_cost_overrides: vec![],
    }
}

/// A deliberately small JSON-Schema subset validator: enough for the
/// keywords these generated schemas use. It resolves `$ref` into `$defs`.
fn schema_accepts(root: &Value, schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference
            .strip_prefix("#/$defs/")
            .ok_or_else(|| format!("{path}: unsupported $ref {reference}"))?;
        let target = root
            .get("$defs")
            .and_then(|defs| defs.get(name))
            .ok_or_else(|| format!("{path}: unresolved $ref {reference}"))?;
        return schema_accepts(root, target, value, path);
    }
    if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
        if any
            .iter()
            .any(|sub| schema_accepts(root, sub, value, path).is_ok())
        {
            return Ok(());
        }
        return Err(format!("{path}: no anyOf branch matched"));
    }
    if let Some(one) = schema.get("oneOf").and_then(Value::as_array) {
        let matches = one
            .iter()
            .filter(|sub| schema_accepts(root, sub, value, path).is_ok())
            .count();
        if matches == 1 {
            return Ok(());
        }
        return Err(format!(
            "{path}: expected exactly one oneOf branch, got {matches}"
        ));
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        for sub in all {
            schema_accepts(root, sub, value, path)?;
        }
    }
    if let Some(constant) = schema.get("const") {
        if constant != value {
            return Err(format!("{path}: expected const {constant}"));
        }
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        if !values.contains(value) {
            return Err(format!("{path}: {value} is not in enum {values:?}"));
        }
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let matches = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            other => return Err(format!("{path}: unsupported type {other}")),
        };
        if !matches {
            return Err(format!("{path}: expected {kind}, got {value}"));
        }
    }
    if let Some(number) = value.as_f64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
            if number < minimum {
                return Err(format!("{path}: {number} < minimum {minimum}"));
            }
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
            if number > maximum {
                return Err(format!("{path}: {number} > maximum {maximum}"));
            }
        }
    }
    if let Some(text) = value.as_str() {
        if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64) {
            if (text.chars().count() as u64) < minimum {
                return Err(format!("{path}: string shorter than {minimum}"));
            }
        }
        if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64) {
            if (text.chars().count() as u64) > maximum {
                return Err(format!("{path}: string longer than {maximum}"));
            }
        }
    }
    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(field) {
                    return Err(format!("{path}: missing required field {field}"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (field, sub) in properties {
                if let Some(field_value) = object.get(field) {
                    schema_accepts(root, sub, field_value, &format!("{path}.{field}"))?;
                }
            }
        }
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {
                let known = schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|properties| properties.keys().collect::<Vec<_>>())
                    .unwrap_or_default();
                for field in object.keys() {
                    if !known.contains(&field) {
                        return Err(format!("{path}: unknown field {field}"));
                    }
                }
            }
            Some(Value::Object(_)) => {}
            _ => return Err(format!("{path}: object schema is not strict")),
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64) {
            if (items.len() as u64) < minimum {
                return Err(format!("{path}: fewer than {minimum} items"));
            }
        }
        if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64) {
            if (items.len() as u64) > maximum {
                return Err(format!("{path}: more than {maximum} items"));
            }
        }
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in items.iter().enumerate() {
                schema_accepts(root, item_schema, item, &format!("{path}[{index}]"))?;
            }
        }
    }
    Ok(())
}

fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let mut current = schema;
    while let Some(reference) = current.get("$ref").and_then(Value::as_str) {
        let name = reference.strip_prefix("#/$defs/").expect("local ref");
        current = root
            .get("$defs")
            .and_then(|defs| defs.get(name))
            .expect("resolved ref");
    }
    current
}

fn enum_values(schema: &Value) -> Vec<&Value> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values.iter().collect();
    }
    schema
        .get("oneOf")
        .and_then(Value::as_array)
        .map(|variants| {
            variants
                .iter()
                .filter_map(|variant| variant.get("const"))
                .collect()
        })
        .unwrap_or_default()
}

fn assert_strict_objects(value: &Value, path: &str) {
    match value {
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("object") {
                match object.get("additionalProperties") {
                    Some(Value::Bool(false)) => {}
                    Some(Value::Object(_)) if !object.contains_key("properties") => {}
                    other => {
                        panic!("{path}: object schema must be strict or a typed map, got {other:?}")
                    }
                }
            }
            for (key, child) in object {
                assert_strict_objects(child, &format!("{path}.{key}"));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                assert_strict_objects(item, &format!("{path}[{index}]"));
            }
        }
        _ => {}
    }
}

#[test]
fn generated_request_schema_is_typed_strict_and_matches_serde() {
    let schema = loop_request_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        required,
        vec![
            "decision_id",
            "revision",
            "engineering_revision",
            "requirement",
            "engineering",
            "candidates",
            "knowledge_domains",
            "inventory",
            "negotiation",
        ],
        "required fields must be exactly the serde-required LoopRequest fields"
    );
    let properties = schema["properties"].as_object().unwrap();
    for optional in [
        "sourcing",
        "require_supplier_fit",
        "assumptions",
        "evidence",
        "relaxations",
    ] {
        assert!(properties.contains_key(optional), "{optional} property");
        assert!(
            !required.contains(&optional),
            "{optional} must stay optional"
        );
    }
    assert_eq!(properties["candidates"]["minItems"], 1);
    assert_eq!(
        properties["candidates"]["maxItems"],
        crate::MAX_LOOP_CANDIDATES as u64
    );
    assert_eq!(
        properties["knowledge_domains"]["maxItems"],
        crate::MAX_LOOP_KNOWLEDGE_DOMAINS as u64
    );
    assert_eq!(
        properties["relaxations"]["maxItems"],
        crate::MAX_LOOP_RELAXATIONS as u64
    );
    assert_eq!(resolve(&schema, &properties["revision"])["minimum"], 1);
    assert_eq!(
        resolve(&schema, &properties["engineering_revision"])["minimum"],
        1
    );
    assert_strict_objects(&schema, "root");

    // Enum vocabulary is generated from the same serde rename rules.
    let incoterms = enum_values(&schema["$defs"]["Incoterm"]);
    for accepted in ["EXW", "FOB", "DDP"] {
        assert!(incoterms
            .iter()
            .any(|value| value.as_str() == Some(accepted)));
        serde_json::from_value::<Incoterm>(json!(accepted)).unwrap();
    }
    assert!(serde_json::from_value::<Incoterm>(json!("TELEPORT")).is_err());
    let modes = enum_values(&schema["$defs"]["TransportMode"]);
    assert!(modes
        .iter()
        .any(|value| value.as_str() == Some("ocean_lcl")));
    serde_json::from_value::<TransportMode>(json!("ocean_lcl")).unwrap();
    assert!(serde_json::from_value::<TransportMode>(json!("teleport")).is_err());

    // Hand-written exact-value schemas carry the same serde bounds.
    assert_eq!(schema["$defs"]["Rate"]["minimum"], 0);
    assert_eq!(
        schema["$defs"]["Rate"]["maximum"],
        faktor_supply_core::MAX_PPM
    );
    assert_eq!(schema["$defs"]["Units"]["minimum"], 0);
    assert_eq!(
        schema["$defs"]["Units"]["maximum"],
        faktor_supply_core::MAX_UNITS
    );
    assert_eq!(schema["$defs"]["Currency"]["pattern"], "^[A-Z]{3}$");
}

#[test]
fn real_loop_request_deserializes_and_validates_against_the_schema() {
    let request = example_request();
    request
        .validate()
        .expect("the fixture is a valid LoopRequest");
    let value = serde_json::to_value(&request).unwrap();
    let schema = loop_request_schema();
    schema_accepts(&schema, &schema, &value, "request").expect("schema must accept the example");
    let decoded: LoopRequest = serde_json::from_value(value).unwrap();
    decoded.validate().unwrap();

    // Every declared required field is load-bearing for serde.
    let required = schema["required"].as_array().unwrap().clone();
    for field in required {
        let field = field.as_str().unwrap();
        let mut missing = serde_json::to_value(&request).unwrap();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<LoopRequest>(missing).is_err(),
            "serde must refuse a document missing {field}"
        );
    }
    // Unknown fields are refused by both serde and the schema.
    let mut unknown = serde_json::to_value(&request).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("attacker_field".to_string(), Value::Bool(true));
    assert!(serde_json::from_value::<LoopRequest>(unknown.clone()).is_err());
    assert!(schema_accepts(&schema, &schema, &unknown, "request").is_err());
}

#[test]
fn tool_schema_generation_is_bounded() {
    let started = std::time::Instant::now();
    let request = loop_request_schema();
    let request_elapsed = started.elapsed();
    let started = std::time::Instant::now();
    let outcome = loop_outcome_schema();
    let outcome_elapsed = started.elapsed();
    let request_bytes = serde_json::to_vec(&request).unwrap().len();
    let outcome_bytes = serde_json::to_vec(&outcome).unwrap().len();
    eprintln!(
        "perf tool schemas: request {request_bytes} bytes in {request_elapsed:?}, outcome {outcome_bytes} bytes in {outcome_elapsed:?}"
    );
    assert!(request_elapsed < std::time::Duration::from_secs(2));
    assert!(outcome_elapsed < std::time::Duration::from_secs(2));
    assert!(request_bytes < 160 * 1024);
    assert!(outcome_bytes < 160 * 1024);
}

#[test]
fn generated_outcome_schema_is_strict_and_complete() {
    let schema = loop_outcome_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        required,
        vec![
            "decision",
            "requirement_set",
            "applied_relaxations",
            "ranked",
            "evaluations",
            "knowledge",
            "receipt",
            "cross_check",
            "binding",
            "performance",
            "authorization",
        ]
    );
    assert_strict_objects(&schema, "outcome");
    let definitions = schema["$defs"].as_object().unwrap();
    assert!(
        definitions.len() >= 100,
        "the verify schema must describe the full outcome graph, got {} definitions",
        definitions.len()
    );
    assert!(definitions.contains_key("EngineeringResponse"));
    assert!(definitions.contains_key("SupplyDecision"));
    assert!(definitions.contains_key("AnalysisReceipt"));
}

/// Typed schemas carry no pure-annotation keywords (`format`, `default`,
/// `$schema`, prose) and stay within their tool-metadata budget. The
/// constraints themselves (`type`, `required`, `minimum`, `enum`, bounds,
/// `additionalProperties`) must all survive: compact is not lossy.
#[test]
fn schemas_are_annotation_free_and_within_their_token_budget() {
    fn walk(value: &Value, property_map: bool, path: &str) {
        match value {
            Value::Object(object) => {
                if !property_map {
                    for annotation in ["format", "default", "$schema"] {
                        assert!(
                            !object.contains_key(annotation),
                            "{path}: schema annotation {annotation:?} is pure token waste"
                        );
                    }
                    assert!(
                        !object.contains_key("title"),
                        "{path}: title is pure token waste"
                    );
                }
                for (key, child) in object {
                    walk(child, key == "properties", &format!("{path}.{key}"));
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk(item, false, &format!("{path}[{index}]"));
                }
            }
            _ => {}
        }
    }
    for (name, schema, max_bytes) in [
        ("request", loop_request_schema(), 56_000usize),
        ("outcome", loop_outcome_schema(), 62_000usize),
    ] {
        walk(&schema, false, name);
        let bytes = serde_json::to_vec(&schema).unwrap().len();
        eprintln!("SCHEMA {name}: bytes={bytes}");
        assert!(
            bytes <= max_bytes,
            "{name} schema is {bytes} bytes; keep the typed contract under {max_bytes}"
        );
        assert_eq!(schema["additionalProperties"], false);
    }
}

#[test]
fn nested_wire_objects_refuse_unknown_fields_like_the_strict_schema() {
    // The generated schema sets additionalProperties:false on every object.
    // These runtime probes pin the corresponding serde deny_unknown_fields
    // attributes, including generic availability and unit containers whose
    // helper Def types are private.
    let mut availability = serde_json::to_value(Availability::known(7_u64)).unwrap();
    availability
        .as_object_mut()
        .unwrap()
        .insert("attacker_field".to_string(), Value::Bool(true));
    assert!(serde_json::from_value::<Availability<u64>>(availability).is_err());

    let mut encoded_quantity = serde_json::to_value(quantity(10.0, "uF")).unwrap();
    encoded_quantity
        .as_object_mut()
        .unwrap()
        .insert("attacker_field".to_string(), Value::Bool(true));
    assert!(serde_json::from_value::<engineering_units::Quantity>(encoded_quantity).is_err());

    let value = engineering_core::EngineeringValue::known(
        quantity(10.0, "uF"),
        engineering_provenance(),
        RevisionId::new(1),
    )
    .unwrap();
    let mut encoded = serde_json::to_value(&value).unwrap();
    encoded
        .as_object_mut()
        .unwrap()
        .insert("attacker_field".to_string(), Value::Bool(true));
    assert!(serde_json::from_value::<engineering_core::EngineeringValue>(encoded).is_err());

    let mut applicability =
        serde_json::to_value(engineering_core::Applicability::unrestricted()).unwrap();
    applicability
        .as_object_mut()
        .unwrap()
        .insert("attacker_field".to_string(), Value::Bool(true));
    assert!(serde_json::from_value::<engineering_core::Applicability>(applicability).is_err());

    let mut action = serde_json::to_value(faktor_supply_core::RecommendedAction::Proceed {
        candidate: CandidateId("candidate-1".to_string()),
    })
    .unwrap();
    action
        .as_object_mut()
        .unwrap()
        .insert("attacker_field".to_string(), Value::Bool(true));
    assert!(serde_json::from_value::<faktor_supply_core::RecommendedAction>(action).is_err());
}

#[test]
fn oversized_or_unbounded_candidate_text_is_refused_at_validation() {
    let mut request = example_request();
    request.candidates[0].processes = vec!["x".repeat(faktor_supply_core::MAX_TEXT_BYTES + 1)];
    assert!(request.validate().is_err());

    let mut request = example_request();
    request.candidates[0].processes =
        vec!["smt".to_string(); crate::MAX_LOOP_CANDIDATE_PROCESSES + 1];
    assert!(request.validate().is_err());

    let mut request = example_request();
    request.negotiation.destination_country = "X".repeat(129);
    assert!(request.validate().is_err());
}
