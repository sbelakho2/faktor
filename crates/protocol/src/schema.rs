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
    let mut out = type_defs_conversation();
    out.extend(type_defs_attachments());
    out.extend(type_defs_task_runs());
    out.extend(type_defs_outcomes());
    out.extend(type_defs_value_entitlements());
    out
}

fn type_defs_conversation() -> Vec<TypeDef> {
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
        // ------------------------------------------------ native wire DTOs
        //
        // Audit 15 migration step 1: the attachment and task-run wire DTOs.
        // Each type mirrors the daemon's own serde/JSON shape exactly; the
        // Rust structs (`faktor_core::attachment::AttachmentId`, the
        // orchestrator's durable run rows / the native handlers) stay the
        // behavioral authority, and the generated TS/Kotlin clients consume
        // these shapes instead of hand-rolled parsers.
    ]
}

fn type_defs_attachments() -> Vec<TypeDef> {
    vec![
    TypeDef {
        name: "AttachmentId",
        doc: "One durable typed binary attachment: BLAKE3 digest (64 lowercase hex), mime, optional original filename and decompressed size. Decoders ignore unknown fields (the native additive-response contract); the daemon's own request DTO rejects them.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                text_field("digest"),
                text_field("mime"),
                nullable("filename", Ty::String),
                req("size", Ty::I64),
            ],
        },
    },
    TypeDef {
        name: "AttachmentRef",
        doc: "One durable attachment REFERENCE (native additive-response contract): the surrogate reference id the retrieval routes address, plus the CAS digest and the exact presentation metadata (mime, optional filename, decompressed size). Decoders ignore unknown fields.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                req("ref_id", Ty::I64),
                text_field("digest"),
                text_field("mime"),
                nullable("filename", Ty::String),
                req("size", Ty::I64),
            ],
        },
    },
    TypeDef {
        name: "AttachmentUpload",
        doc: "Strict request body of POST /native/session/{id}/attachments: canonical standard base64 bytes plus the declared mime.",
        unknown_fields: "reject",
        shape: Shape::Struct {
            fields: vec![
                text_field("mime"),
                field("filename", Ty::String, true, true),
                text_field("data_base64"),
            ],
        },
    },
    ]
}

fn type_defs_task_runs() -> Vec<TypeDef> {
    vec![
    TypeDef {
        name: "TaskRun",
        doc: "One durable task run projection (native task-run list/state reads; additive: decoders ignore unknown fields).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                req("task_id", Ty::I64),
                text_field("run_id"),
                text_field("mode"),
                text_field("state"),
                nullable("goal", Ty::String),
                req(
                    "item_ids",
                    Ty::List {
                        of: Box::new(Ty::String),
                    },
                ),
                nullable("model", Ty::String),
            ],
        },
    },
    TypeDef {
        name: "TaskRunStarted",
        doc: "Response of a native task-run start: the durable run identity and its current state (additive: unknown fields ignored).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                req("task_id", Ty::I64),
                text_field("run_id"),
                text_field("state"),
            ],
        },
    },
    TypeDef {
        name: "TaskRunCancelled",
        doc: "Response of a native task-run cancel (additive: unknown fields ignored).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![text_field("run_id"), req("cancelled", Ty::Bool)],
        },
    },
    TypeDef {
        name: "TaskRunWorkItem",
        doc: "One wire work item of a native task-run start (the orchestrator's own JSON vocabulary).",
        unknown_fields: "reject",
        shape: Shape::Struct {
            fields: vec![
                text_field("id"),
                text_field("kind"),
                field("summary", Ty::String, true, true),
                field(
                    "depends_on",
                    Ty::List {
                        of: Box::new(Ty::String),
                    },
                    true,
                    true,
                ),
                field(
                    "acceptance_checks",
                    Ty::List {
                        of: Box::new(Ty::String),
                    },
                    true,
                    true,
                ),
                field("ownership", Ty::Json, true, true),
                field("required_capabilities", Ty::Json, true, true),
            ],
        },
    },
    TypeDef {
        name: "TaskRunStartRequest",
        doc: "Strict request body of POST /native/session/{id}/task-runs. The daemon validates every member strictly; money accepts a decimal string or a lossless JSON integer and is documented as json here. `submission_id` is the required client submission UUID of the logical start (idempotency key: a repeated key replays the original run receipt byte-for-byte).",
        unknown_fields: "reject",
        shape: Shape::Struct {
            fields: vec![
                text_field("goal"),
                text_field("submission_id"),
                field(
                    "criteria",
                    Ty::List {
                        of: Box::new(Ty::String),
                    },
                    true,
                    true,
                ),
                field(
                    "work_items",
                    Ty::List {
                        of: Box::new(Ty::Named {
                            name: "TaskRunWorkItem",
                        }),
                    },
                    true,
                    true,
                ),
                field("ownership", Ty::Json, true, true),
                field("model", Ty::String, true, true),
                field("max_tokens", Ty::I64, true, true),
                field("max_cost_micro", Ty::Json, true, true),
                field("mutation_mode", Ty::String, true, true),
                field("routing_mode", Ty::String, true, true),
                field(
                    "files",
                    Ty::List {
                        of: Box::new(Ty::String),
                    },
                    true,
                    true,
                ),
                field(
                    "attachments",
                    Ty::List {
                        of: Box::new(Ty::Named {
                            name: "AttachmentId",
                        }),
                    },
                    true,
                    true,
                ),
                field("completion_contract", Ty::Json, true, true),
            ],
        },
    },
    TypeDef {
        name: "SessionPromptRequest",
        doc: "Strict request body of POST /native/session/{id}/prompt. `submission_id` is the required client submission UUID of the logical prompt (idempotency key: a repeated key with the equal normalized body returns the original receipt byte-for-byte; a different body under the same key is a typed conflict). The daemon validates every member strictly (`deny_unknown_fields`; an unknown field or typo is a 400).",
        unknown_fields: "reject",
        shape: Shape::Struct {
            fields: vec![
                text_field("session_id"),
                text_field("submission_id"),
                text_field("prompt"),
                field(
                    "files",
                    Ty::List {
                        of: Box::new(Ty::String),
                    },
                    true,
                    true,
                ),
            ],
        },
    },
    TypeDef {
        name: "SessionPromptReceipt",
        doc: "Response of a native prompt: the accepted in-session run identity and queued flag (additive: unknown fields ignored). A submission-keyed replay returns the stored receipt byte-for-byte.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                text_field("op_id"),
                text_field("run_id"),
                req("accepted", Ty::Bool),
                req("queued", Ty::Bool),
            ],
        },
    },
    // ---------------------------------- engineering outcome ledger (audit 7)
    //
    // The Engineering Outcome Ledger read surface (`GET /native/outcomes`)
    // projects the durable `faktor_agent::outcome_ledger` records into a
    // camelCase wire response. The schema mirrors that projection
    // field-for-field (see `crates/server/src/native/outcomes.rs`): money
    // keys ride the decimal-string `*Micro` encoding, every other number
    // is a plain JSON integer and non-nullable. The strict scalar query
    // (`limit` / `before_ms` / `before_run`) has no JSON body, so it is
    // not a schema DTO.
    ]
}

fn type_defs_outcomes() -> Vec<TypeDef> {
    vec![
    TypeDef {
        name: "OutcomeMetrics",
        doc: "The Engineering Outcome Ledger's first-class aggregate metrics plus the fold's scan honesty (`scanned` / `truncated`). Money fields are decimal strings (`*Micro`) — or null when the fact is not settled: `avoidedHostedInferenceMicro` is null with `avoidedHostedInferenceMeasurement = \"unavailable\"` unless at least one run carried a MEASURED counterfactual (zero is only a measured zero). Monetary metrics fold only `moneyCompleteRuns`; `moneyPendingRuns` / `moneyUnavailableRuns` are excluded and counted by `moneyIncompleteExcluded`, and `verifiedChangesMoneyComplete` is the shared Successful-Run-Cost/VCSC denominator (a client must gate BOTH cost values on it, never render its empty-denominator zero). Every rate carries its served denominator so an empty denominator renders as not-measured, never a measured 0: `localOnlyRuns`, `localFinalRuns`, `localFirstRuns`, `runsWithSteps` (the Hosted-Only Rate denominator: runs with at least one model step, `hostedOnlyRuns` / `runsWithSteps`), `runs`, `checksRequired`, and `totalSpendMicro` (Rework Cost Ratio); the Frontier Escalation Rate is null unless durable execution-class authority exists and its `measured` state folds over `runs` while `partial` folds over `frontierClassedRuns` (`frontierEscalationClassedRuns` / `frontierClassedRuns`). `verifiedFirstVerificationPassChanges` / `verifiedFirstVerificationPassPpm` are the Verified First Verification Pass Rate: verified changes whose FIRST VERIFICATION attempt passed — a verification-attempt measure, never an implementation-attempt claim. `localToHostedFallback*` is the Local-to-Hosted Fallback Rate and `hostedOnly*` the Hosted-Only Rate; both classify steps by the durable `executionClass` only. `frontierEscalationMeasurement` reports `measured` | `partial` | `unmeasured`. Local classification coverage mirrors that rule: `localMeasurement` reports `measured` | `partial` | `unmeasured` with the boundary-classed cohort in `localClassedRuns` (the boundary-derived local cohorts can only classify runs inside it — a folded display sequence never silently depresses them). `attributionCompleteRuns` / `attributionPartialRuns` / `attributionUnavailableRuns` count the recorded provider/model attribution honesty; `attributionIncompleteExcluded` (partial + unavailable) is the routing-learning feed exclusion count — a non-complete run never fed the ledger's atomic feed (its samples kept the per-call feed, never dropped).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                req("runs", Ty::I64),
                req("verifiedChanges", Ty::I64),
                req("verifiedFirstVerificationPassChanges", Ty::I64),
                req("verifiedFirstVerificationPassPpm", Ty::I64),
                req("moneyCompleteRuns", Ty::I64),
                req("moneyPendingRuns", Ty::I64),
                req("moneyUnavailableRuns", Ty::I64),
                req("moneyIncompleteExcluded", Ty::I64),
                req("verifiedChangesMoneyComplete", Ty::I64),
                text_field("totalVerifiedSpendMicro"),
                text_field("successfulRunCostMicro"),
                text_field("totalSpendMicro"),
                text_field("verifiedCostPerSuccessfulChangeMicro"),
                text_field("totalReworkSpendMicro"),
                req("reworkCostRatioPpm", Ty::I64),
                    req("localRuns", Ty::I64),
                    req("localOnlyRuns", Ty::I64),
                    req("localOnlyVerifiedChanges", Ty::I64),
                    req("localOnlyVerifiedPpm", Ty::I64),
                    req("localFinalRuns", Ty::I64),
                    req("localFinalVerifiedChanges", Ty::I64),
                    req("localFinalVerifiedPpm", Ty::I64),
                req("localFirstRuns", Ty::I64),
                req("localFirstSuccessChanges", Ty::I64),
                req("localFirstSuccessPpm", Ty::I64),
                req("localToHostedFallbackRuns", Ty::I64),
                req("localToHostedFallbackPpm", Ty::I64),
                req("localClassedRuns", Ty::I64),
                text_field("localMeasurement"),
                req("hostedOnlyRuns", Ty::I64),
                req("runsWithSteps", Ty::I64),
                req("hostedOnlyPpm", Ty::I64),
                nullable("avoidedHostedInferenceMicro", Ty::String),
                text_field("avoidedHostedInferenceMeasurement"),
                req("avoidedHostedMeasuredRuns", Ty::I64),
                req("attributionCompleteRuns", Ty::I64),
                req("attributionPartialRuns", Ty::I64),
                req("attributionUnavailableRuns", Ty::I64),
                req("attributionIncompleteExcluded", Ty::I64),
                req("totalWallTimeMs", Ty::I64),
                req("meanTimeToVerifiedChangeMs", Ty::I64),
                req("humanInterventionRuns", Ty::I64),
                req("humanInterventionPpm", Ty::I64),
                req("rollbackRuns", Ty::I64),
                req("regressionRollbackPpm", Ty::I64),
                req("checksPassed", Ty::I64),
                req("checksRequired", Ty::I64),
                req("proofCoveragePpm", Ty::I64),
                req("frontierEscalationRuns", Ty::I64),
                req("frontierClassedRuns", Ty::I64),
                req("frontierEscalationClassedRuns", Ty::I64),
                nullable("frontierEscalationPpm", Ty::I64),
                text_field("frontierEscalationMeasurement"),
                req("scanned", Ty::I64),
                req("truncated", Ty::Bool),
            ],
        },
    },
    TypeDef {
        name: "RunOutcomeModelStep",
        doc: "One provider/model step of a completed run's execution sequence. `executionClass` is the durable route-time classification (`local` | `hosted_economy` | `hosted_balanced` | `frontier` | `unknown`); `local` is CLASS-DERIVED — true only when the class is `local`. A step with no durable class authority (`unknown`: a pre-class legacy payload or an unpriced path) exposes its recorded pre-class local bit as `legacyLocal` (true only for a classless step whose stored row recorded a local execution); a class-carrying step always reports `legacyLocal` false, and the legacy bit is never promoted to a class claim. `unknown` is counted as neither local nor hosted by the metrics. `spendMicro` is a decimal string only when the run's money is settled (`complete`); null otherwise — the money is not final, never a fabricated zero.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                text_field("provider"),
                text_field("model"),
                req("local", Ty::Bool),
                req("legacyLocal", Ty::Bool),
                text_field("executionClass"),
                nullable("spendMicro", Ty::String),
            ],
        },
    },
    TypeDef {
        name: "RunOutcomeProof",
        doc: "One completed run's proof coverage: required checks, passed checks and the clamped coverage ppm.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                req("checksPassed", Ty::I64),
                req("checksRequired", Ty::I64),
                req("coveragePpm", Ty::I64),
            ],
        },
    },
    TypeDef {
        name: "RunOutcomeChangeScale",
        doc: "One completed run's changed-code scale.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                req("files", Ty::I64),
                req("addedLines", Ty::I64),
                req("removedLines", Ty::I64),
            ],
        },
    },
    TypeDef {
        name: "RunOutcome",
        doc: "One completed run's wire projection in the Engineering Outcome Ledger page: identity, acceptance/integration/verification verdict, model sequence, the LATEST settled money projection (`moneyState`), wall time, interventions, proof, change scale, rollback/recovery counters and the final verification basis. Engineering facts are immutable from record time; money fields are decimal strings only when `moneyState == \"complete\"` and null otherwise (a pending/unavailable money fact is never a fabricated zero). `avoidedHostedMicro` is null with `avoidedHostedMeasurement = \"unavailable\"` when no counterfactual authority exists. `verifiedFirstVerificationPass` is the run's FIRST VERIFICATION attempt having passed (a verification-attempt measure, never a first-implementation-attempt claim). `firstExecutionClass` / `finalExecutionClass` are the run's exactly-preserved boundary execution classes (the classification authority, independent of the bounded `modelSequence` folding; a pre-field payload falls back to the preserved sequence boundary — `unknown` when the authority is absent). `attributionState` reports `complete` | `partial` | `unavailable`: a non-complete attribution records the run but never fed its samples into the ledger's routing-learning feed (they kept the per-call feed).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                text_field("runId"),
                req("sessionId", Ty::I64),
                req("taskId", Ty::I64),
                text_field("taskClass"),
                text_field("riskClass"),
                nullable("candidateSnapshot", Ty::String),
                text_field("finalSnapshot"),
                req("accepted", Ty::Bool),
                req("integrated", Ty::Bool),
                req("verified", Ty::Bool),
                req("verifiedFirstVerificationPass", Ty::Bool),
                req("repairAttempts", Ty::I64),
                req(
                    "modelSequence",
                    Ty::List {
                        of: Box::new(Ty::Named {
                            name: "RunOutcomeModelStep",
                        }),
                    },
                ),
                text_field("firstExecutionClass"),
                text_field("finalExecutionClass"),
                text_field("attributionState"),
                text_field("moneyState"),
                nullable("modelSpendMicro", Ty::String),
                nullable("verificationSpendMicro", Ty::String),
                nullable("reworkSpendMicro", Ty::String),
                nullable("avoidedHostedMicro", Ty::String),
                text_field("avoidedHostedMeasurement"),
                req("wallTimeMs", Ty::I64),
                req("humanInterventions", Ty::I64),
                req(
                    "proof",
                    Ty::Named {
                        name: "RunOutcomeProof",
                    },
                ),
                req(
                    "changeScale",
                    Ty::Named {
                        name: "RunOutcomeChangeScale",
                    },
                ),
                req("rollbackEvents", Ty::I64),
                req("recoveryEvents", Ty::I64),
                text_field("finalBasis"),
                req("frontierEscalations", Ty::I64),
                req("completedAtMs", Ty::I64),
            ],
        },
    },
    TypeDef {
        name: "OutcomeCursor",
        doc: "The composite oldest-key cursor of one Engineering Outcome Ledger page (`nextCursor`); absent (null) when the page was not full.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![req("completedAtMs", Ty::I64), text_field("runId")],
        },
    },
    TypeDef {
        name: "OutcomesPage",
        doc: "Response of GET /native/outcomes: the durable metrics fold plus one bounded newest-first page of completed-run projections and the composite cursor older pages continue from (null when exhausted).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                req(
                    "metrics",
                    Ty::Named {
                        name: "OutcomeMetrics",
                    },
                ),
                req(
                    "runs",
                    Ty::List {
                        of: Box::new(Ty::Named { name: "RunOutcome" }),
                    },
                ),
                nullable(
                    "nextCursor",
                    Ty::Named {
                        name: "OutcomeCursor",
                    },
                ),
            ],
        },
    },
    ]
}

fn type_defs_value_entitlements() -> Vec<TypeDef> {
    vec![
    TypeDef {
        name: "ValueGovernanceCapabilities",
        doc: "The value-entitlement report's governance capability picture (`governance`): `sso`/`scim` are capability vocabulary, effective only behind the `enterprise_governance` value entitlement (never an implementation).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![req("sso", Ty::Bool), req("scim", Ty::Bool)],
        },
    },
    TypeDef {
        name: "ValueEntitlementDecision",
        doc: "One per-entitlement value decision of the report: the entitlement tag, its typed enforcement hook, whether it is enforceable now and (only when not) the exact refusal cause. A granted decision omits `cause` (the schema field is optional).",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                text_field("entitlement"),
                text_field("hook"),
                req("granted", Ty::Bool),
                field("cause", Ty::String, true, true),
            ],
        },
    },
    TypeDef {
        name: "ValueEntitlementsReport",
        doc: "The caller organization's DERIVED value-entitlement report (`value_entitlements` member of GET /native/value-entitlements): the plan resolution, the plan-mapped inference-independent value tags, the effective governance capability flags and one typed decision per entitlement. The report never folds usage, credits or tokens.",
        unknown_fields: "ignore",
        shape: Shape::Struct {
            fields: vec![
                text_field("organization_id"),
                nullable("plan_id", Ty::String),
                req("plan_found", Ty::Bool),
                req("subscription_active", Ty::Bool),
                req(
                    "plan_entitlements",
                    Ty::List {
                        of: Box::new(Ty::String),
                    },
                ),
                req(
                    "governance",
                    Ty::Named {
                        name: "ValueGovernanceCapabilities",
                    },
                ),
                req(
                    "decisions",
                    Ty::List {
                        of: Box::new(Ty::Named {
                            name: "ValueEntitlementDecision",
                        }),
                    },
                ),
                req("now_ms", Ty::I64),
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

    /// Audit 15: the migrated native wire DTOs must STAY in the canonical
    /// schema — removing one is a generated-client regression, not a label
    /// change.
    #[test]
    fn migrated_native_dtos_are_in_the_canonical_schema() {
        let names: Vec<&str> = type_defs().iter().map(|def| def.name).collect();
        for name in [
            "AttachmentId",
            "AttachmentRef",
            "AttachmentUpload",
            "TaskRun",
            "TaskRunStarted",
            "TaskRunCancelled",
            "TaskRunWorkItem",
            "TaskRunStartRequest",
        ] {
            assert!(
                names.contains(&name),
                "audit 15 migration: {name} must stay in the canonical schema"
            );
        }
    }

    /// Economic audit item 7: the Engineering Outcome Ledger read DTOs must
    /// STAY in the canonical schema — the live wire response shape is pinned
    /// against these entries by the server route test
    /// (`crates/server/src/native/outcomes_tests.rs`), so removing one is a
    /// generated-client regression, not a label change.
    #[test]
    fn outcomes_wire_dtos_are_in_the_canonical_schema() {
        let types = type_defs();
        let names: Vec<&str> = types.iter().map(|def| def.name).collect();
        for name in [
            "OutcomeMetrics",
            "RunOutcomeModelStep",
            "RunOutcomeProof",
            "RunOutcomeChangeScale",
            "RunOutcome",
            "OutcomeCursor",
            "OutcomesPage",
        ] {
            assert!(
                names.contains(&name),
                "outcomes migration: {name} must stay in the canonical schema"
            );
        }
        // Money rides the decimal-string `*Micro` wire encoding: those fields
        // must stay strings, never integers.
        let string_fields = |type_name: &str| -> Vec<&'static str> {
            let def = types
                .iter()
                .find(|def| def.name == type_name)
                .expect("type is present");
            let Shape::Struct { fields } = &def.shape else {
                panic!("{type_name} must be a struct")
            };
            fields
                .iter()
                .filter(|f| matches!(f.ty, Ty::String))
                .map(|f| f.name)
                .collect()
        };
        assert!(string_fields("OutcomeMetrics").contains(&"totalVerifiedSpendMicro"));
        assert!(string_fields("OutcomeMetrics").contains(&"avoidedHostedInferenceMicro"));
        assert!(string_fields("RunOutcomeModelStep").contains(&"spendMicro"));
        for money in [
            "modelSpendMicro",
            "verificationSpendMicro",
            "reworkSpendMicro",
            "avoidedHostedMicro",
        ] {
            assert!(
                string_fields("RunOutcome").contains(&money),
                "RunOutcome.{money} must be a decimal-string field"
            );
        }
    }

    /// The value-entitlement report DTOs (commercial value gate) must STAY
    /// in the canonical schema — the live wire response shape is pinned
    /// against these entries by the server route test
    /// (`crates/server/src/native/value_tests.rs`), so removing one is a
    /// generated-client regression, not a label change.
    #[test]
    fn value_entitlements_wire_dtos_are_in_the_canonical_schema() {
        let types = type_defs();
        let names: Vec<&str> = types.iter().map(|def| def.name).collect();
        for name in [
            "ValueEntitlementsReport",
            "ValueEntitlementDecision",
            "ValueGovernanceCapabilities",
        ] {
            assert!(
                names.contains(&name),
                "value-entitlement migration: {name} must stay in the canonical schema"
            );
        }
        let def = |name: &str| -> TypeDef {
            types
                .iter()
                .find(|def| def.name == name)
                .unwrap_or_else(|| panic!("{name} must be in the canonical schema"))
                .clone()
        };
        let field = |name: &str, field_name: &str| -> Field {
            let Shape::Struct { fields } = def(name).shape else {
                panic!("{name} must be a struct")
            };
            fields
                .into_iter()
                .find(|f| f.name == field_name)
                .unwrap_or_else(|| panic!("{name}.{field_name} must exist"))
        };
        // Granted decisions OMIT the cause key: the schema field must stay
        // optional (and null is accepted by the generated decoders).
        let cause = field("ValueEntitlementDecision", "cause");
        assert!(
            cause.optional,
            "cause must stay optional (skip_serializing_if)"
        );
        assert!(cause.nullable, "cause accepts JSON null");
        // `plan_id` is always present, null when nothing resolved.
        let plan_id = field("ValueEntitlementsReport", "plan_id");
        assert!(
            plan_id.nullable && !plan_id.optional,
            "plan_id: null, never absent"
        );
        // The plan tags and the decisions are string arrays / struct lists.
        let plan_entitlements = field("ValueEntitlementsReport", "plan_entitlements");
        assert!(matches!(
            plan_entitlements.ty,
            Ty::List { ref of } if matches!(of.as_ref(), Ty::String)
        ));
        let decisions = field("ValueEntitlementsReport", "decisions");
        assert!(matches!(
            decisions.ty,
            Ty::List { ref of }
                if matches!(
                    of.as_ref(),
                    Ty::Named {
                        name: "ValueEntitlementDecision"
                    }
                )
        ));
    }

    /// The `AttachmentId` schema entry mirrors the REAL serde struct
    /// (`faktor_core::attachment::AttachmentId`) field-for-field, including
    /// the `deny_unknown_fields` strictness: a schema/wire mismatch is a
    /// generated-client bug.
    #[test]
    fn attachment_id_schema_mirrors_the_real_serde_shape() {
        use faktor_core::attachment::AttachmentId;
        use faktor_core::hash::FileHash;

        let value = AttachmentId {
            digest: FileHash::from([7u8; 32]),
            mime: "image/png".into(),
            filename: Some("shot.png".into()),
            size: 4,
        };
        let json = serde_json::to_value(&value).unwrap();
        let object = json
            .as_object()
            .expect("AttachmentId serializes to an object");
        let mut wire_keys: Vec<&str> = object.keys().map(String::as_str).collect();
        wire_keys.sort_unstable();
        let def = type_defs()
            .into_iter()
            .find(|def| def.name == "AttachmentId")
            .expect("AttachmentId is part of the canonical schema");
        let Shape::Struct { fields } = def.shape else {
            panic!("AttachmentId must be a struct")
        };
        let mut schema_keys: Vec<&str> = fields.iter().map(|f| f.name).collect();
        schema_keys.sort_unstable();
        assert_eq!(
            wire_keys, schema_keys,
            "schema must mirror every serde field"
        );
        assert!(
            object["digest"].is_string(),
            "digest is the hex wire string"
        );
        let without_name = AttachmentId {
            digest: FileHash::from([7u8; 32]),
            mime: "image/png".into(),
            filename: None,
            size: 4,
        };
        let without_name = serde_json::to_value(&without_name).unwrap();
        assert!(
            without_name["filename"].is_null(),
            "an absent filename serializes as an explicit null (the nullable schema field)"
        );
        // `deny_unknown_fields` on the real type and the decoder contract
        // differ BY DESIGN: the daemon's request DTO rejects a foreign member,
        // while the generated decoders ignore unknown fields (the native
        // additive-response contract). Both facts are pinned here.
        let mut with_extra = object.clone();
        with_extra.insert("extra".into(), serde_json::json!(1));
        assert!(serde_json::from_value::<AttachmentId>(with_extra.into()).is_err());
        assert_eq!(def.unknown_fields, "ignore");
    }

    /// The `AttachmentRef` schema entry mirrors the REAL serde struct
    /// (`faktor_core::attachment::AttachmentRef`) field-for-field: the upload
    /// response and the ref-addressed retrieval routes both speak this shape,
    /// so the surrogate reference id must be part of the canonical schema.
    #[test]
    fn attachment_ref_schema_mirrors_the_real_serde_shape() {
        use faktor_core::attachment::{AttachmentId, AttachmentRef};
        use faktor_core::hash::FileHash;

        let value = AttachmentRef {
            id: 7,
            digest: FileHash::from([7u8; 32]),
            mime: "image/png".into(),
            filename: Some("shot.png".into()),
            size: 4,
        };
        let json = serde_json::to_value(&value).unwrap();
        let object = json
            .as_object()
            .expect("AttachmentRef serializes to an object");
        let mut wire_keys: Vec<&str> = object.keys().map(String::as_str).collect();
        wire_keys.sort_unstable();
        let def = type_defs()
            .into_iter()
            .find(|def| def.name == "AttachmentRef")
            .expect("AttachmentRef is part of the canonical schema");
        let Shape::Struct { fields } = def.shape else {
            panic!("AttachmentRef must be a struct")
        };
        let mut schema_keys: Vec<&str> = fields.iter().map(|f| f.name).collect();
        schema_keys.sort_unstable();
        assert_eq!(
            wire_keys, schema_keys,
            "schema must mirror every serde field"
        );
        assert!(
            object["ref_id"].is_i64(),
            "the surrogate id is the ref_id wire field"
        );
        assert_eq!(object["digest"].as_str().unwrap().len(), 64);
        // The reference projects to its full attachment identity, whose own
        // schema entry is the AttachmentId.
        let projected: AttachmentId = value.attachment();
        assert_eq!(projected.digest, value.digest);
        assert_eq!(projected.size, value.size);
        assert_eq!(def.unknown_fields, "ignore");
    }
}
