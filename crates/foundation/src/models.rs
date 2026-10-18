//! BK02 model parameter contract: adapter-supported descriptors, one
//! compile step that rejects unsupported values BEFORE dispatch, and durable
//! normalized call receipts (requested vs effective) that are immutable once
//! written.

use std::collections::BTreeMap;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use faktor_core::model::{CapabilityAuthority, ModelCapabilities};

use crate::db::FoundationDb;
use crate::{page_limit_i64, FoundationError};

pub const MAX_CALL_RECEIPT_PAGE: usize = 200;
pub const MAX_PARAMETER_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_STOP_SEQUENCES: usize = 4;
pub const MAX_STOP_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParameterKind {
    Number {
        min: f64,
        max: f64,
    },
    Integer {
        min: i64,
        max: i64,
    },
    Choices {
        choices: Vec<String>,
    },
    Toggle,
    StringList {
        max_items: usize,
        max_item_bytes: usize,
    },
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterDescriptor {
    pub name: String,
    pub label: String,
    pub kind: ParameterKind,
    /// True when the parameter requires model-SPECIFIC authority (a
    /// generic declaration is not enough to certify it).
    pub requires_exact_authority: bool,
    /// False when the capability gate disables the control; `unavailable_reason`
    /// then names the missing prerequisite.
    pub available: bool,
    pub unavailable_reason: Option<String>,
    /// The parameter's default is the provider's, never a fabricated zero.
    pub default_is_inherited: bool,
    pub apply_timing: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelTuningDescriptor {
    pub provider: String,
    pub model: String,
    pub authority: String,
    pub capabilities_gated: bool,
    pub context: u64,
    pub max_output: u64,
    pub parameters: BTreeMap<String, ParameterDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveParameter {
    pub value: Value,
    /// `requested` | `inherited`
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveRequest {
    pub provider: String,
    pub model: String,
    pub parameters: BTreeMap<String, EffectiveParameter>,
    /// Parameters the caller did not set; the provider default applies and
    /// is NOT represented as `0`/`false`.
    pub inherited: Vec<String>,
    pub capability_revision: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallReceipt {
    pub session_id: u64,
    pub call_id: String,
    pub op_id: String,
    pub provider: String,
    pub model: String,
    pub route: String,
    pub capability_revision: u64,
    pub requested: BTreeMap<String, Value>,
    pub effective: BTreeMap<String, Value>,
    pub inherited: Vec<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub status: String,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub error: Option<String>,
}

/// Build the served descriptor for one exact model. Capability authority
/// gates features exactly like dispatch: `unknown` authority leaves
/// tools/vision/schema parameters unavailable (with a reason), never enabled
/// because the provider family "usually" supports them.
pub fn tuning_descriptor(
    provider: &str,
    model: &str,
    caps: &ModelCapabilities,
    authority: CapabilityAuthority,
    gated: bool,
) -> ModelTuningDescriptor {
    let exact = authority == CapabilityAuthority::Known;
    let mut parameters = BTreeMap::new();
    macro_rules! add {
        ($descriptor:expr) => {{
            let descriptor = $descriptor;
            parameters.insert(descriptor.name.clone(), descriptor);
        }};
    }
    let inherited =
        |name: &str, label: &str, kind: ParameterKind, exact_required: bool| ParameterDescriptor {
            name: name.to_string(),
            label: label.to_string(),
            kind,
            requires_exact_authority: exact_required,
            available: !exact_required || exact,
            unavailable_reason: if exact_required && !exact {
                Some("exact model capability authority required".into())
            } else {
                None
            },
            default_is_inherited: true,
            apply_timing: "next_call".into(),
        };
    add!(inherited(
        "maxOutput",
        "Maximum output",
        ParameterKind::Integer {
            min: 1,
            max: caps.max_output.max(1) as i64,
        },
        false,
    ));
    add!(inherited(
        "temperature",
        "Temperature",
        ParameterKind::Number { min: 0.0, max: 2.0 },
        false,
    ));
    add!(inherited(
        "topP",
        "Top-p",
        ParameterKind::Number { min: 0.0, max: 1.0 },
        false,
    ));
    add!(inherited(
        "topK",
        "Top-k",
        ParameterKind::Integer { min: 1, max: 1_000 },
        true,
    ));
    add!(inherited(
        "frequencyPenalty",
        "Frequency penalty",
        ParameterKind::Number {
            min: -2.0,
            max: 2.0
        },
        true,
    ));
    add!(inherited(
        "presencePenalty",
        "Presence penalty",
        ParameterKind::Number {
            min: -2.0,
            max: 2.0
        },
        true,
    ));
    add!(inherited(
        "seed",
        "Seed",
        ParameterKind::Integer {
            min: 0,
            max: u32::MAX as i64,
        },
        true,
    ));
    add!(inherited(
        "stop",
        "Stop sequences",
        ParameterKind::StringList {
            max_items: MAX_STOP_SEQUENCES,
            max_item_bytes: MAX_STOP_BYTES,
        },
        false,
    ));
    add!(inherited(
        "reasoning",
        "Reasoning effort",
        ParameterKind::Choices {
            choices: ["off", "low", "medium", "high"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        true,
    ));
    if !caps.reasoning {
        if let Some(d) = parameters.get_mut("reasoning") {
            d.available = false;
            d.unavailable_reason = Some("model reports no reasoning support".into());
        }
    }
    add!(inherited(
        "thinkingBudget",
        "Thinking token budget",
        ParameterKind::Integer {
            min: 0,
            max: caps.max_output.max(1) as i64,
        },
        true,
    ));
    if !caps.thinking {
        if let Some(d) = parameters.get_mut("thinkingBudget") {
            d.available = false;
            d.unavailable_reason = Some("model reports no thinking support".into());
        }
    }
    add!(inherited(
        "parallelTools",
        "Parallel tool calls",
        ParameterKind::Toggle,
        false,
    ));
    if !caps.supports_parallel_tools() {
        if let Some(d) = parameters.get_mut("parallelTools") {
            d.available = false;
            d.unavailable_reason = Some(
                if caps.tools {
                    "model does not declare parallel tool calling"
                } else {
                    "model reports no tool support"
                }
                .into(),
            );
        }
    }
    add!(inherited(
        "outputSchema",
        "Structured output schema",
        ParameterKind::Json,
        true,
    ));
    if !caps.json_schema || gated {
        if let Some(d) = parameters.get_mut("outputSchema") {
            d.available = false;
            d.unavailable_reason = Some(if gated {
                "capabilities are gated for an unknown model".into()
            } else {
                "model reports no structured-output support".into()
            });
        }
    }
    add!(inherited(
        "serviceTier",
        "Service tier",
        ParameterKind::Choices {
            choices: ["auto", "default", "flex", "priority"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        true,
    ));
    add!(inherited(
        "cachePolicy",
        "Cache policy",
        ParameterKind::Choices {
            choices: ["auto", "off", "explicit"]
                .into_iter()
                .map(str::to_string)
                .collect(),
        },
        true,
    ));
    let mut stream = inherited("stream", "Stream responses", ParameterKind::Toggle, false);
    if !caps.streaming {
        stream.available = false;
        stream.unavailable_reason = Some("model reports no streaming support".into());
    }
    add!(stream);
    ModelTuningDescriptor {
        provider: provider.to_string(),
        model: model.to_string(),
        authority: authority.as_str().to_string(),
        capabilities_gated: gated,
        context: caps.context as u64,
        max_output: caps.max_output as u64,
        parameters,
    }
}

/// Compile a requested parameter bundle against the model descriptor.
/// Returns EVERY error (unknown/disabled/out-of-range/cross-field), never a
/// partial effective request.
pub fn compile_effective_request(
    descriptor: &ModelTuningDescriptor,
    requested: &BTreeMap<String, Value>,
    capability_revision: u64,
) -> Result<EffectiveRequest, Vec<String>> {
    let mut errors = Vec::new();
    let mut parameters = BTreeMap::new();
    let mut warnings = Vec::new();
    for (name, value) in requested {
        let Some(spec) = descriptor.parameters.get(name) else {
            errors.push(format!("{name}: unsupported parameter"));
            continue;
        };
        if !spec.available {
            errors.push(format!(
                "{name}: {}",
                spec.unavailable_reason
                    .as_deref()
                    .unwrap_or("parameter is unavailable")
            ));
            continue;
        }
        if spec.requires_exact_authority && descriptor.authority != "known" {
            errors.push(format!(
                "{name}: exact capability authority required (authority={})",
                descriptor.authority
            ));
            continue;
        }
        if let Err(message) = validate_parameter(spec, value) {
            errors.push(format!("{name}: {message}"));
            continue;
        }
        parameters.insert(
            name.clone(),
            EffectiveParameter {
                value: value.clone(),
                source: "requested".into(),
            },
        );
    }
    // Cross-field rules.
    if let (Some(thinking), Some(max_output)) = (
        requested.get("thinkingBudget").and_then(Value::as_i64),
        requested.get("maxOutput").and_then(Value::as_i64),
    ) {
        if thinking > max_output {
            errors.push("thinkingBudget: must not exceed maxOutput".into());
        }
    }
    if requested.contains_key("outputSchema")
        && !descriptor
            .parameters
            .get("outputSchema")
            .map(|d| d.available)
            .unwrap_or(false)
    {
        errors.push("outputSchema: structured output is unavailable for this model".into());
    }
    if requested
        .get("parallelTools")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !descriptor
            .parameters
            .get("parallelTools")
            .map(|d| d.available)
            .unwrap_or(false)
    {
        errors.push("parallelTools: parallel tool calling is unavailable".into());
    }
    if requested.contains_key("seed") {
        warnings.push(
            "seed: determinism is best-effort; providers may not honor it across versions".into(),
        );
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let inherited = descriptor
        .parameters
        .keys()
        .filter(|name| !parameters.contains_key(*name))
        .cloned()
        .collect();
    Ok(EffectiveRequest {
        provider: descriptor.provider.clone(),
        model: descriptor.model.clone(),
        parameters,
        inherited,
        capability_revision,
        warnings,
    })
}

fn validate_parameter(spec: &ParameterDescriptor, value: &Value) -> Result<(), String> {
    let encoded = serde_json::to_vec(value).map_err(|e| format!("value is not JSON: {e}"))?;
    if encoded.len() > MAX_PARAMETER_VALUE_BYTES {
        return Err(format!(
            "value is {} bytes, over {MAX_PARAMETER_VALUE_BYTES}",
            encoded.len()
        ));
    }
    match &spec.kind {
        ParameterKind::Number { min, max } => {
            let Some(n) = value.as_f64() else {
                return Err("expected a number".into());
            };
            if !n.is_finite() || n < *min || n > *max {
                return Err(format!("{n} is outside the served range {min}..={max}"));
            }
            Ok(())
        }
        ParameterKind::Integer { min, max } => {
            let Some(n) = value.as_i64() else {
                return Err("expected an integer".into());
            };
            if n < *min || n > *max {
                return Err(format!("{n} is outside the served range {min}..={max}"));
            }
            Ok(())
        }
        ParameterKind::Choices { choices } => {
            let Some(s) = value.as_str() else {
                return Err("expected a string choice".into());
            };
            if !choices.iter().any(|c| c == s) {
                return Err(format!("{s:?} is not one of {}", choices.join(", ")));
            }
            Ok(())
        }
        ParameterKind::Toggle => value
            .is_boolean()
            .then_some(())
            .ok_or_else(|| "expected a boolean".into()),
        ParameterKind::StringList {
            max_items,
            max_item_bytes,
        } => {
            let Some(items) = value.as_array() else {
                return Err("expected a list of strings".into());
            };
            if items.len() > *max_items {
                return Err(format!("{} entries, over {max_items}", items.len()));
            }
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err("list entries must be strings".into());
                };
                if s.len() > *max_item_bytes {
                    return Err(format!("entry is {} bytes, over {max_item_bytes}", s.len()));
                }
            }
            Ok(())
        }
        ParameterKind::Json => Ok(()),
    }
}

/// Durable call receipts. A call id is unique per session; replaying the
/// SAME receipt is idempotent, a different body under the same id conflicts.
pub struct CallReceiptStore {
    db: FoundationDb,
}

impl CallReceiptStore {
    pub fn new(db: FoundationDb) -> Self {
        Self { db }
    }

    pub fn record(&self, receipt: &CallReceipt) -> Result<CallReceipt, FoundationError> {
        let params_json = serde_json::to_string(&receipt.requested)
            .map_err(|e| FoundationError::Store(format!("encode requested: {e}")))?;
        let effective_json = serde_json::to_string(&receipt.effective)
            .map_err(|e| FoundationError::Store(format!("encode effective: {e}")))?;
        let inherited_json = serde_json::to_string(&receipt.inherited)
            .map_err(|e| FoundationError::Store(format!("encode inherited: {e}")))?;
        self.db.write("call receipt", |conn| {
            let existing: Option<(String, String, String, String, String, String)> = conn
                .query_row(
                    "SELECT parameters_json, requested_json, status, provider, model, route
                     FROM call_receipt
                     WHERE session_id = ?1 AND call_id = ?2",
                    params![receipt.session_id as i64, receipt.call_id],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            if let Some((effective, requested, status, provider, model, route)) = existing {
                if effective == effective_json
                    && requested == params_json
                    && status == receipt.status
                    && provider == receipt.provider
                    && model == receipt.model
                    && route == receipt.route
                {
                    return self.read_one(conn, receipt.session_id, &receipt.call_id);
                }
                return Err(FoundationError::Conflict(format!(
                    "call receipt {} already exists with different content",
                    receipt.call_id
                )));
            }
            conn.execute(
                "INSERT INTO call_receipt(
                   session_id, op_id, call_id, provider, model, route, capability_revision,
                   parameters_json, requested_json, inherited_json, started_ms, ended_ms, status,
                   tokens_in, tokens_out, error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    receipt.session_id as i64,
                    receipt.op_id,
                    receipt.call_id,
                    receipt.provider,
                    receipt.model,
                    receipt.route,
                    receipt.capability_revision as i64,
                    effective_json,
                    params_json,
                    inherited_json,
                    receipt.started_ms,
                    receipt.ended_ms,
                    receipt.status,
                    receipt.tokens_in.map(|v| v as i64),
                    receipt.tokens_out.map(|v| v as i64),
                    receipt.error,
                ],
            )
            .map_err(|e| FoundationError::Store(e.to_string()))?;
            self.read_one(conn, receipt.session_id, &receipt.call_id)
        })
    }

    pub fn page(
        &self,
        session_id: u64,
        after_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<CallReceipt>, FoundationError> {
        let limit = page_limit_i64(limit.clamp(1, MAX_CALL_RECEIPT_PAGE), "call receipt")?;
        self.db.read("call receipts page", |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, session_id, op_id, call_id, provider, model, route,
                            capability_revision, parameters_json, requested_json, inherited_json,
                            started_ms, ended_ms, status, tokens_in, tokens_out, error
                     FROM call_receipt
                     WHERE session_id = ?1 AND id > ?2
                     ORDER BY id ASC LIMIT ?3",
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            let rows = stmt
                .query_map(
                    params![
                        i64::try_from(session_id).map_err(|_| FoundationError::Oversized(
                            "call receipt session id exceeds SQLite signed range".into()
                        ))?,
                        after_id.unwrap_or(0),
                        limit
                    ],
                    map_receipt,
                )
                .map_err(|e| FoundationError::Store(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| FoundationError::Store(e.to_string()))?;
            Ok(rows)
        })
    }

    fn read_one(
        &self,
        conn: &rusqlite::Connection,
        session_id: u64,
        call_id: &str,
    ) -> Result<CallReceipt, FoundationError> {
        conn.query_row(
            "SELECT id, session_id, op_id, call_id, provider, model, route,
                    capability_revision, parameters_json, requested_json, inherited_json,
                    started_ms, ended_ms, status, tokens_in, tokens_out, error
             FROM call_receipt WHERE session_id = ?1 AND call_id = ?2",
            params![session_id as i64, call_id],
            map_receipt,
        )
        .map_err(|e| FoundationError::Store(e.to_string()))
    }
}

fn map_receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallReceipt> {
    let effective_raw: String = row.get(8)?;
    let requested_raw: String = row.get(9)?;
    let inherited_raw: String = row.get(10)?;
    let inherited: Vec<String> = serde_json::from_str(&inherited_raw).unwrap_or_default();
    let route: String = row.get(6)?;
    Ok(CallReceipt {
        session_id: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        op_id: row.get(2)?,
        call_id: row.get(3)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        route,
        capability_revision: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
        effective: serde_json::from_str(&effective_raw).unwrap_or_default(),
        requested: serde_json::from_str(&requested_raw).unwrap_or_default(),
        inherited,
        started_ms: row.get(11)?,
        ended_ms: row.get(12)?,
        status: row.get(13)?,
        tokens_in: row
            .get::<_, Option<i64>>(14)?
            .and_then(|v| u64::try_from(v).ok()),
        tokens_out: row
            .get::<_, Option<i64>>(15)?
            .and_then(|v| u64::try_from(v).ok()),
        error: row.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::model::CapabilityAuthority;

    fn caps() -> ModelCapabilities {
        ModelCapabilities {
            context: 32_768,
            max_output: 4_096,
            tools: true,
            parallel_tools: true,
            thinking: true,
            vision: true,
            json_schema: true,
            streaming: true,
            embeddings: false,
            reasoning: true,
        }
    }

    #[test]
    fn unset_and_zero_temperature_are_distinct_and_unsupported_fields_fail_locally() {
        let descriptor = tuning_descriptor("p", "m", &caps(), CapabilityAuthority::Known, false);
        let empty = compile_effective_request(&descriptor, &BTreeMap::new(), 1).unwrap();
        assert!(empty.parameters.is_empty());
        assert!(empty.inherited.contains(&"temperature".to_string()));
        let zero = compile_effective_request(
            &descriptor,
            &BTreeMap::from([("temperature".to_string(), Value::from(0.0))]),
            1,
        )
        .unwrap();
        assert_eq!(zero.parameters["temperature"].value, Value::from(0.0));
        assert!(!zero.inherited.contains(&"temperature".to_string()));
        let err = compile_effective_request(
            &descriptor,
            &BTreeMap::from([("bogus".to_string(), Value::from(1))]),
            1,
        )
        .unwrap_err();
        assert!(err[0].contains("unsupported parameter"));
        let err = compile_effective_request(
            &descriptor,
            &BTreeMap::from([("temperature".to_string(), Value::from(9.0))]),
            1,
        )
        .unwrap_err();
        assert!(err[0].contains("outside the served range"));
    }

    #[test]
    fn unknown_authority_gates_exact_parameters() {
        let descriptor = tuning_descriptor("p", "m", &caps(), CapabilityAuthority::Unknown, true);
        assert!(!descriptor.parameters["reasoning"].available);
        assert!(!descriptor.parameters["outputSchema"].available);
        assert_eq!(descriptor.authority, "unknown");
        let err = compile_effective_request(
            &descriptor,
            &BTreeMap::from([("reasoning".to_string(), Value::from("high"))]),
            1,
        )
        .unwrap_err();
        assert!(err[0].contains("unavailable") || err[0].contains("authority"));
    }

    #[test]
    fn receipts_are_idempotent_immutable_and_paged() {
        let dir = tempfile::tempdir().unwrap();
        let db = FoundationDb::open(dir.path().join("f.db")).unwrap();
        let store = CallReceiptStore::new(db);
        let receipt = CallReceipt {
            session_id: 7,
            call_id: "call-1".into(),
            op_id: "op-1".into(),
            provider: "p".into(),
            model: "m".into(),
            route: "economy".into(),
            capability_revision: 3,
            requested: BTreeMap::from([("temperature".to_string(), Value::from(0.0))]),
            effective: BTreeMap::from([("temperature".to_string(), Value::from(0.0))]),
            inherited: vec!["topP".into()],
            started_ms: 1,
            ended_ms: None,
            status: "admitted".into(),
            tokens_in: None,
            tokens_out: None,
            error: None,
        };
        let first = store.record(&receipt).unwrap();
        assert_eq!(first.inherited, vec!["topP".to_string()]);
        assert_eq!(store.record(&receipt).unwrap().call_id, "call-1");
        let mut changed = receipt.clone();
        changed.provider = "other".into();
        assert!(store.record(&changed).is_err());
        let page = store.page(7, None, 10).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].effective["temperature"], Value::from(0.0));
        assert!(store.page(8, None, 10).unwrap().is_empty());
    }
}
