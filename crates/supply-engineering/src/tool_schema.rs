//! Typed JSON Schemas for the daemon Supply/Engineering tools.
//!
//! The schemas are generated from the authoritative request/result types by
//! `schemars`, so a field, enum, numeric bound or `deny_unknown_fields`
//! attribute cannot drift between the Rust wire type and the advertised tool
//! contract. Every object schema is strict (`additionalProperties: false`) and
//! the root `$ref` is inlined (with `$defs` retained) so each tool exposes an
//! object schema directly.

use schemars::{schema_for, Schema};
use serde_json::Value;

use crate::{LoopOutcome, LoopRequest};

fn compact(value: &mut Value, property_map: bool) {
    match value {
        Value::Object(object) => {
            // Long doc comments are useful in Rust, not in a tool contract;
            // keeping only the structural keywords keeps the schema compact.
            // A `properties` map is not a schema: its keys are field names
            // (including legal fields named `description` or `title`).
            if !property_map {
                if object.contains_key("properties") {
                    object.insert("additionalProperties".to_string(), Value::Bool(false));
                }
                // Annotations, not constraints: the Rust wire type already
                // carries both (serde applies `default`, and `format` is a
                // non-validating annotation). They are pure schema tokens.
                object.remove("$schema");
                object.remove("default");
                object.remove("description");
                object.remove("format");
                object.remove("title");
            }
            for (key, child) in object.iter_mut() {
                compact(child, key == "properties");
            }
            if !property_map {
                let const_variants = object
                    .get("oneOf")
                    .and_then(Value::as_array)
                    .filter(|variants| {
                        !variants.is_empty()
                            && variants.iter().all(|variant| {
                                variant.as_object().is_some_and(|fields| {
                                    fields.keys().all(|key| key == "const" || key == "type")
                                        && fields.get("const").is_some_and(Value::is_string)
                                })
                            })
                    })
                    .map(|variants| {
                        variants
                            .iter()
                            .filter_map(|variant| variant.get("const").cloned())
                            .collect::<Vec<_>>()
                    });
                if let Some(values) = const_variants {
                    object.remove("oneOf");
                    object.insert("type".to_string(), Value::String("string".to_string()));
                    object.insert("enum".to_string(), Value::Array(values));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                compact(item, false);
            }
        }
        _ => {}
    }
}

fn inline_root(schema: Schema) -> Value {
    let value = serde_json::to_value(schema).expect("a generated schema is JSON");
    let Some(object) = value.as_object() else {
        return value;
    };
    let Some(reference) = object.get("$ref").and_then(Value::as_str) else {
        return value;
    };
    let Some(name) = reference.strip_prefix("#/$defs/") else {
        return value;
    };
    let Some(definition) = object
        .get("$defs")
        .and_then(Value::as_object)
        .and_then(|defs| defs.get(name))
        .and_then(Value::as_object)
    else {
        return value;
    };
    let mut root = definition.clone();
    if let Some(defs) = object.get("$defs") {
        root.insert("$defs".to_string(), defs.clone());
    }
    Value::Object(root)
}

fn generated<T: schemars::JsonSchema>() -> Value {
    let mut value = inline_root(schema_for!(T));
    compact(&mut value, false);
    value
}

/// Input schema for `supply_decision` and `engineering_analyse`.
pub fn loop_request_schema() -> Value {
    generated::<LoopRequest>()
}

/// Input schema for `engineering_verify` (a stored `LoopOutcome`).
pub fn loop_outcome_schema() -> Value {
    generated::<LoopOutcome>()
}

#[cfg(test)]
mod tests;
