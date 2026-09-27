//! Canonical protocol schema (audit 25).
//!
//! [`canonical`] is the ONE machine-readable description of the plain
//! `faktor-protocol` DTOs ([`crate::native`]) and the frozen API error-code
//! constants ([`crate::error`]). It is emitted deterministically to
//! `crates/protocol/schema/faktor-protocol.schema.json` by the
//! `faktor-protocol-schema` binary; `scripts/protocol-codegen.mjs`
//! regenerates the TypeScript/Kotlin DTO+parser portions from that artifact
//! and diffs them against the checked-in clients (CI fails on drift).
//!
//! The error-code rows are DERIVED from [`crate::error::from_core`]
//! behavior, never duplicated by hand: the row list below constructs one
//! representative [`faktor_core::error::ErrorKind`] per variant and records
//! what `from_core` answers. The exhaustive `kind_tag` match makes a new
//! `ErrorKind` variant a compile error here until it is given a row.
//!
//! Native protocol semantics are unchanged: the schema mirrors the serde
//! shapes in [`crate::native`] exactly (field names, enum tags, nullability,
//! `deny_unknown_fields` strictness).

use faktor_core::error::{Error, ErrorKind};
use serde::Serialize;

pub const SCHEMA_ID: &str = "faktor-protocol-schema/v1";
pub const GENERATOR: &str = "crates/protocol/src/schema.rs";
pub const ARTIFACT_PATH: &str = "crates/protocol/schema/faktor-protocol.schema.json";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProtocolSchema {
    pub schema: &'static str,
    pub generator: &'static str,
    pub types: Vec<TypeDef>,
    pub constants: Constants,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TypeDef {
    pub name: &'static str,
    pub doc: &'static str,
    /// What a decoder does with fields it does not know: the Rust serde
    /// attribute, verbatim (`reject` = `deny_unknown_fields`).
    pub unknown_fields: &'static str,
    pub shape: Shape,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shape {
    Struct {
        fields: Vec<Field>,
    },
    TaggedEnum {
        tag: &'static str,
        rename_all: &'static str,
        variants: Vec<Variant>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Field {
    pub name: &'static str,
    #[serde(rename = "type")]
    pub ty: Ty,
    /// `true` when the value may be JSON `null` (`Option` without skip).
    pub nullable: bool,
    /// `true` when the key may be ABSENT (serde `default` /
    /// `skip_serializing_if`).
    pub optional: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Variant {
    pub name: &'static str,
    /// The wire value of the `type` tag.
    pub tag: &'static str,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ty {
    String,
    I64,
    I32,
    Bool,
    Json,
    Named { name: &'static str },
    List { of: Box<Ty> },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Constants {
    pub error_envelope: ErrorEnvelope,
    pub error_codes: Vec<ErrorCode>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorEnvelope {
    pub fields: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ErrorCode {
    /// The core error variant family the row was derived from.
    pub core_kind: &'static str,
    pub code: &'static str,
    pub http_status: u16,
    pub retryable: bool,
}

fn field(name: &'static str, ty: Ty, nullable: bool, optional: bool) -> Field {
    Field {
        name,
        ty,
        nullable,
        optional,
    }
}

fn req(name: &'static str, ty: Ty) -> Field {
    field(name, ty, false, false)
}

fn nullable(name: &'static str, ty: Ty) -> Field {
    field(name, ty, true, false)
}

fn text_field(name: &'static str) -> Field {
    req(name, Ty::String)
}

/// The plain DTOs, in the source order of [`crate::native`].
fn type_defs() -> Vec<TypeDef> {
    vec![
        TypeDef {
            name: "Message",
            doc: "One conversation message row (parts nested).",
            unknown_fields: "ignore",
            shape: Shape::Struct {
                fields: vec![
                    text_field("id"),
                    // "user" | "assistant" | "system" (validated by callers).
                    text_field("role"),
                    text_field("session_id"),
                    req("seq", Ty::I64),
                    req("created_ms", Ty::I64),
                    req(
                        "parts",
                        Ty::List {
                            of: Box::new(Ty::Named { name: "Part" }),
                        },
                    ),
                ],
            },
        },
        TypeDef {
            name: "Part",
            doc: "A typed message part: text, reasoning, tool call, tool result or summary.",
            unknown_fields: "reject",
            shape: Shape::TaggedEnum {
                tag: "type",
                rename_all: "snake_case",
                variants: vec![
                    Variant {
                        name: "Text",
                        tag: "text",
                        fields: vec![text_field("text")],
                    },
                    Variant {
                        name: "Reasoning",
                        tag: "reasoning",
                        fields: vec![text_field("text")],
                    },
                    Variant {
                        name: "ToolCall",
                        tag: "tool_call",
                        fields: vec![
                            text_field("tool_call_id"),
                            text_field("name"),
                            req("input", Ty::Json),
                            text_field("state"),
                        ],
                    },
                    Variant {
                        name: "ToolResult",
                        tag: "tool_result",
                        fields: vec![
                            text_field("tool_call_id"),
                            req(
                                "result",
                                Ty::Named {
                                    name: "ToolResultBody",
                                },
                            ),
                        ],
                    },
                    Variant {
                        name: "Summary",
                        tag: "summary",
                        fields: vec![text_field("text")],
                    },
                ],
            },
        },
        TypeDef {
            name: "ToolResultBody",
            doc: "Bounded tool result: excerpt, exit code, artifact pointer and slice hint.",
            unknown_fields: "reject",
            shape: Shape::Struct {
                fields: vec![
                    text_field("excerpt"),
                    nullable("exit_code", Ty::I32),
                    nullable("artifact", Ty::String),
                    nullable("slice_hint", Ty::String),
                ],
            },
        },
        TypeDef {
            name: "PageMeta",
            doc: "Additive paging metadata: applied size, next cursor, has_more, total estimate.",
            unknown_fields: "ignore",
            shape: Shape::Struct {
                fields: vec![
                    req("size", Ty::I64),
                    nullable("cursor", Ty::I64),
                    req("has_more", Ty::Bool),
                    // #[serde(default, skip_serializing_if = "Option::is_none")]
                    field("total_estimate", Ty::I64, true, true),
                ],
            },
        },
        TypeDef {
            name: "MessagesPage",
            doc: "One bounded page of conversation messages.",
            unknown_fields: "ignore",
            shape: Shape::Struct {
                fields: vec![
                    text_field("session_id"),
                    req(
                        "messages",
                        Ty::List {
                            of: Box::new(Ty::Named { name: "Message" }),
                        },
                    ),
                    req("has_more", Ty::Bool),
                    nullable("next_before", Ty::I64),
                    // #[serde(default)] page: PageMeta
                    field("page", Ty::Named { name: "PageMeta" }, false, true),
                ],
            },
        },
        TypeDef {
            name: "SessionState",
            doc: "The durable session state projection.",
            unknown_fields: "ignore",
            shape: Shape::Struct {
                fields: vec![
                    text_field("session_id"),
                    text_field("state"),
                    text_field("title"),
                    req("last_event_seq", Ty::I64),
                    req(
                        "agent_state",
                        Ty::Named {
                            name: "AgentStateView",
                        },
                    ),
                    nullable("task_ledger", Ty::Json),
                ],
            },
        },
        TypeDef {
            name: "AgentStateView",
            doc: "The agent state machine view folded into a session state row.",
            unknown_fields: "ignore",
            shape: Shape::Struct {
                fields: vec![
                    text_field("state"),
                    text_field("label"),
                    req("active", Ty::Bool),
                    req("terminal", Ty::Bool),
                ],
            },
        },
    ]
}

/// One representative value per [`ErrorKind`] variant. The match in
/// [`kind_tag`] (below) is exhaustive: a new variant is a compile error
/// here until it is listed.
fn error_kinds() -> Vec<ErrorKind> {
    use faktor_core::state::AgentState;
    vec![
        ErrorKind::NotFound,
        ErrorKind::Conflict,
        ErrorKind::InvalidState {
            from: AgentState::Idle,
            to: AgentState::Completed,
        },
        ErrorKind::Permission,
        ErrorKind::Timeout,
        ErrorKind::Cancelled,
        ErrorKind::Store,
        ErrorKind::Network,
        ErrorKind::Provider {
            code: "provider".into(),
            retryable: true,
        },
        ErrorKind::Malformed,
        ErrorKind::Oversized,
        ErrorKind::RateLimited,
        ErrorKind::Deadlock,
        ErrorKind::Internal,
    ]
}

/// Exhaustive tag of [`ErrorKind`] (a new variant fails to compile here
/// until it is added to [`error_kinds`] too).
fn kind_tag(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::Conflict => "conflict",
        ErrorKind::InvalidState { .. } => "invalid_state",
        ErrorKind::Permission => "permission",
        ErrorKind::Timeout => "timeout",
        ErrorKind::Cancelled => "cancelled",
        ErrorKind::Store => "store",
        ErrorKind::Network => "network",
        ErrorKind::Provider { .. } => "provider",
        ErrorKind::Malformed => "malformed",
        ErrorKind::Oversized => "oversized",
        ErrorKind::RateLimited => "rate_limited",
        ErrorKind::Deadlock => "deadlock",
        ErrorKind::Internal => "internal",
    }
}

/// The frozen error-code table, derived from actual [`crate::error::from_core`]
/// behavior (never a hand-maintained copy).
pub fn error_codes() -> Vec<ErrorCode> {
    error_kinds()
        .into_iter()
        .map(|kind| {
            let tag = kind_tag(&kind);
            let api = crate::error::from_core(&Error::new(kind, "schema"));
            ErrorCode {
                core_kind: tag,
                code: api.code,
                http_status: api.http_status,
                retryable: api.retryable,
            }
        })
        .collect()
}

/// The canonical schema artifact.
pub fn canonical() -> ProtocolSchema {
    ProtocolSchema {
        schema: SCHEMA_ID,
        generator: GENERATOR,
        types: type_defs(),
        constants: Constants {
            error_envelope: ErrorEnvelope {
                fields: vec!["code", "message", "retryable"],
            },
            error_codes: error_codes(),
        },
    }
}

/// Deterministic artifact text (stable field order, trailing newline).
pub fn canonical_json() -> String {
    let mut text =
        serde_json::to_string_pretty(&canonical()).expect("protocol schema serialization");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_cover_every_kind_once() {
        let rows = error_codes();
        assert_eq!(rows.len(), error_kinds().len());
        let mut tags: Vec<&str> = rows.iter().map(|r| r.core_kind).collect();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), rows.len(), "duplicate core_kind rows");
        let mut codes: Vec<&str> = rows.iter().map(|r| r.code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), rows.len(), "duplicate error codes");
        for row in &rows {
            assert!(
                (100..=599).contains(&(row.http_status as u32)),
                "{}: implausible status {}",
                row.code,
                row.http_status
            );
        }
        let by_code = |code: &str| rows.iter().find(|r| r.code == code).unwrap();
        assert_eq!(by_code("not_found").http_status, 404);
        assert!(by_code("timeout").retryable);
        assert!(!by_code("cancelled").retryable);
    }

    #[test]
    fn type_defs_are_internally_consistent() {
        let types = type_defs();
        let names: Vec<&str> = types.iter().map(|t| t.name).collect();
        for def in &types {
            let fields: Vec<&Field> = match &def.shape {
                Shape::Struct { fields } => fields.iter().collect(),
                Shape::TaggedEnum { variants, .. } => {
                    variants.iter().flat_map(|v| v.fields.iter()).collect()
                }
            };
            for f in fields {
                if let Ty::Named { name } = &f.ty {
                    assert!(
                        names.contains(name),
                        "{}: field {} references unknown type {}",
                        def.name,
                        f.name,
                        name
                    );
                }
                if let Ty::List { of } = &f.ty {
                    if let Ty::Named { name } = of.as_ref() {
                        assert!(
                            names.contains(name),
                            "{}: field {} references unknown type {}",
                            def.name,
                            f.name,
                            name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn canonical_json_is_deterministic_and_stable() {
        let first = canonical_json();
        assert_eq!(first, canonical_json(), "artifact must be byte-stable");
        assert!(first.ends_with("}\n"), "trailing newline contract");
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(value["schema"], SCHEMA_ID);
        assert_eq!(value["generator"], GENERATOR);
    }
}
