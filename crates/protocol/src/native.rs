//! Faktor-native protocol types (`docs/native-protocol.md`).
//!
//! These are the daemon's OWN shapes: the conversation view (messages,
//! parts), cursor paging metadata and the session state projection shared
//! by the session layer, the ACP adapter and the HTTP surface. They are
//! Faktor-owned strict structures — never a frozen foreign wire contract.
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir

use serde::{Deserialize, Serialize};

// ------------------------------------------------------------------ messages

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub role: String, // "user" | "assistant" | "system"
    pub session_id: String,
    pub seq: i64,
    pub created_ms: i64,
    pub parts: Vec<Part>,
    /// The turn/operation identity that produced this message (audit item 6:
    /// the UI groups conversation by the AUTHORITY that produced a message
    /// instead of role adjacency — an assistant answer, its tool calls and
    /// results share one turn id). ADDITIVE: `None` (serialized away) for
    /// every legacy durable row that predates the field — history is never
    /// rewritten — and for projections whose durable message row does not
    /// yet carry an operation id; the live event frames already carry
    /// `turn`, so new projections can populate this without a migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Part {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        name: String,
        input: serde_json::Value,
        state: String,
    },
    ToolResult {
        tool_call_id: String,
        result: ToolResultBody,
    },
    Summary {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolResultBody {
    /// Last N lines, error lines, exit code, important matches — never the
    /// full unbounded output.
    pub excerpt: String,
    pub exit_code: Option<i32>,
    pub artifact: Option<String>,
    /// Pointer into the durable artifact for slice reads.
    pub slice_hint: Option<String>,
}

// --------------------------------------------------------------------- paging

/// Additive paging metadata (paging is fundamental): every paged read
/// response carries an explicit `page` object so clients can prove a page
/// was bounded and learn, without guessing, whether more pages follow.
/// Cursors are stable positions: replaying one returns the same window and
/// appends never reorder pages behind a cursor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PageMeta {
    /// The page size the server applied (the requested limit after
    /// clamping) — the bound this page proves. Echoed even on empty and
    /// final pages so an empty response is unambiguous.
    pub size: i64,
    /// Cursor identifying the position right AFTER this page (the next
    /// page starts there); `null` when this page is the final one. Pass it
    /// back verbatim to the next page request.
    pub cursor: Option<i64>,
    /// True when at least one more page follows this one.
    pub has_more: bool,
    /// Total entries when the server computed it without unbounded work;
    /// `null` when unknown (never paid on the hot read path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_estimate: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessagesPage {
    pub session_id: String,
    pub messages: Vec<Message>,
    /// True when older messages exist (there is another page).
    pub has_more: bool,
    pub next_before: Option<i64>,
    /// Additive paging metadata mirroring `has_more`/`next_before` and
    /// adding the applied page size.
    #[serde(default)]
    pub page: PageMeta,
}

// -------------------------------------------------------- execution kind

/// The execution semantics ONE prompt asks for (audit P0-ASK).
///
/// The wire field is `execution` on the strict ordinary-prompt DTO:
/// `"task"` (the DEFAULT, byte-compatible with every client that predates
/// the field — an older client's prompt keeps today's engineering-task
/// behavior with no silent change) or `"ask"` (a conversational turn).
///
/// This is a PROTOCOL-level vocabulary shared by the DTO, the executor's
/// admission and the agent runtime's drive — one enum, never a per-crate
/// re-spelling. Ask and task share the same runtime, permission,
/// cancellation, recovery and ownership infrastructure; the kind only
/// selects which POLICY the shared authority applies (see
/// [`ExecutionKind::is_ask`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    /// An engineering task run: implementation work items, isolation
    /// (shadow) admission, the engineering contract, verification and the
    /// completion gate. The default for older clients.
    #[default]
    Task,
    /// A conversational ask: a durable conversational turn with NO
    /// implementation work item, NO shadow copy, NO engineering contract
    /// and NO verification/completion gate. Read-permitted tools only.
    Ask,
}

impl ExecutionKind {
    /// The stable wire spelling (`"task"` / `"ask"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            ExecutionKind::Task => "task",
            ExecutionKind::Ask => "ask",
        }
    }

    /// True for the default engineering-task semantics.
    pub const fn is_task(self) -> bool {
        matches!(self, ExecutionKind::Task)
    }

    /// True for conversational-ask semantics.
    pub const fn is_ask(self) -> bool {
        matches!(self, ExecutionKind::Ask)
    }
}

/// Durable memory-fact kind of the per-TURN execution row written by task
/// admission for runs whose semantics differ from the engineering default
/// (today: `ask`). Keyed by [`turn_execution_row_key`]; the row's ABSENCE
/// is the documented legacy default (`task`) for every turn admitted
/// before the field existed or by an unkeyed/direct path — never a guess
/// about a present-but-undecodable value (a corrupt row is a loud typed
/// refusal at decode).
pub const TURN_EXECUTION_ROW_KIND: &str = "turn_execution";

/// The memory-fact key of ONE turn's execution row: `turn-exec:<op>` (the
/// turn's durable operation id, zero-padded hex). Keyed by the OP — never
/// by the session — so a queued ask behind an in-flight turn can never
/// overwrite the execution semantics of the turn being driven.
pub fn turn_execution_row_key(op_raw: u64) -> String {
    format!("turn-exec:{op_raw:016x}")
}

/// The durable execution row of ONE admitted turn: the execution kind plus
/// the turn's accepted attachment identities (ask runs carry theirs HERE,
/// on the turn, because they create no engineering task row; task runs
/// keep the historical task-row attachment set). Strict: a present row
/// that does not decode shape-typed is a loud refusal, never a fallback
/// default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnExecution {
    pub execution: ExecutionKind,
    /// The turn's accepted `AttachmentId` set (CAS bytes by digest, never
    /// workspace paths). Empty = the attachment-free conversational turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<faktor_core::attachment::AttachmentId>,
}

impl TurnExecution {
    /// Encode this row as its durable JSON value.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Decode one durable row value. A malformed/tampered value is a typed
    /// error — the caller refuses the turn loudly, never a silent default.
    pub fn decode(value: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(value)
    }
}

// ------------------------------------------------------- lazy turn intent

/// The lazily-classified intent of ONE ordinary chat turn (audit item 4:
/// contracts/objectives are activated per turn, never by a user-selected
/// mode). The deterministic classifier (faktor-orchestrator's
/// `task_executor::intent`) produces this verdict at admission; it is
/// persisted on its OWN op-keyed row — SEPARATE from any previous task —
/// so the decision is durable history and never folds into a task ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnIntent {
    /// The turn stays conversational end-to-end: no contract, no shadow,
    /// no completion gate, and no engineering task projection rides its
    /// provider request.
    Conversational,
    /// The turn establishes an EngineeringObjective+Contract mid-turn
    /// through the SAME executor admission path (same authority, no second
    /// agent): explicit mutation/fix/implement verbs, user-supplied
    /// contract/verification fields, or an explicit start-task escalation.
    EstablishContract,
    /// A follow-up ("continue" / short pronoun reference) that RESOLVES
    /// against the conversation's ACTIVE engineering objective when one
    /// exists — it never creates a new objective. Without an active
    /// objective the admission records [`TurnIntent::Conversational`]
    /// instead (the resolver's documented zero).
    ContinueObjective,
}

impl TurnIntent {
    /// The stable row spelling (`"conversational"` / `"establish_contract"`
    /// / `"continue_objective"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            TurnIntent::Conversational => "conversational",
            TurnIntent::EstablishContract => "establish_contract",
            TurnIntent::ContinueObjective => "continue_objective",
        }
    }

    /// True when the turn runs with engineering-task semantics (it
    /// established or resolved against an objective).
    pub const fn is_engineering(self) -> bool {
        matches!(
            self,
            TurnIntent::EstablishContract | TurnIntent::ContinueObjective
        )
    }
}

/// Durable memory-fact kind of the per-TURN intent row written by ask
/// admission for every classified ordinary chat turn (today: every
/// `execution: ask` admission). Keyed by [`turn_intent_row_key`]; the row's
/// ABSENCE is the documented legacy default (a task run admitted before the
/// classifier existed, or an unkeyed/direct drive) — never a guess about a
/// present-but-undecodable value (a corrupt row is a loud typed refusal at
/// decode).
pub const TURN_INTENT_ROW_KIND: &str = "turn_intent";

/// The memory-fact key of ONE turn's intent row: `turn-intent:<op>` (the
/// turn's durable operation id, zero-padded hex). Keyed by the OP — never
/// by the session — so a queued turn behind an in-flight one can never
/// overwrite the intent of the turn being driven, and the row never binds
/// to (or mutates) any task identity.
pub fn turn_intent_row_key(op_raw: u64) -> String {
    format!("turn-intent:{op_raw:016x}")
}

/// The durable intent row of ONE classified turn: the verdict itself plus
/// the execution semantics the verdict RESOLVED to (a promoted
/// [`TurnIntent::EstablishContract`] / [`TurnIntent::ContinueObjective`]
/// row carries `task`; a conversational one carries `ask`). Strict: a
/// present row that does not decode shape-typed is a loud refusal, never a
/// fallback default.
///
/// ADDITIVE `promotion_objective`: the bounded engineering objective the
/// model stated through the [`PROMOTION_REQUEST_TOOL`] signal when THIS
/// verdict consumed an armed deferred promotion (see
/// [`TurnPromotionRow`]). Absent (serialized away, decodes as `None`) on
/// every row that was not produced by a promotion consumption — legacy
/// durable bytes stay byte-identical and history is never rewritten. A
/// present-but-invalid objective (empty / over
/// [`MAX_PROMOTION_OBJECTIVE_BYTES`]) is a loud typed refusal at decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnIntentRow {
    pub intent: TurnIntent,
    pub execution: ExecutionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promotion_objective: Option<String>,
}

impl TurnIntentRow {
    /// Build a row with no promotion provenance (the legacy shape).
    pub fn new(intent: TurnIntent, execution: ExecutionKind) -> Self {
        Self {
            intent,
            execution,
            promotion_objective: None,
        }
    }

    /// Encode this row as its durable JSON value.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Decode one durable row value. A malformed/tampered value is a typed
    /// error — the caller refuses the turn loudly, never a silent default.
    /// A present `promotion_objective` must satisfy the SAME strict bound
    /// the signal itself enforces ([`validate_promotion_objective`]).
    pub fn decode(value: &str) -> Result<Self, serde_json::Error> {
        let row: Self = serde_json::from_str(value)?;
        if let Some(objective) = &row.promotion_objective {
            validate_promotion_objective(objective).map_err(serde::de::Error::custom)?;
        }
        Ok(row)
    }
}

// ------------------------------------------------------- deferred promotion
//
// The ONE deterministic mid-turn escalation seam of the no-mode turn
// architecture (the classifier can miss — non-English, unusual
// phrasing). A conversational turn whose model discovers it needs
// engineering CALLS the reserved [`PROMOTION_REQUEST_TOOL`]; the agent's
// tool loop intercepts it before any execution, answers the typed
// [`PROMOTION_DEFERRED_TAG`] refusal and arms the durable
// [`TurnPromotionRow`] marker. The marker is consumed (exactly once, CAS)
// by the NEXT ask admission's classifier consult, which upgrades a
// conversational verdict to [`TurnIntent::EstablishContract`] — the SAME
// `TaskExecutor` admission authority, no second agent, no duplicated turn
// or charge. Nothing about the signaling turn itself changes.

/// The reserved tool-call name of the promotion signal. The name is
/// RESERVED in the agent's tool loop: the signal is intercepted by name
/// before any registry lookup, permission hop or execution, so an
/// executing tool can never ride it — even if a registry registers this
/// name (the conversational bundle carries the protocol-owned spec
/// instead).
pub const PROMOTION_REQUEST_TOOL: &str = "promotion_request";

/// Hard byte bound of the required `objective` field (bounded everything).
///
/// Sized so the WORST-CASE JSON encoding of the whole
/// [`TurnPromotionRow`] value (every objective byte escaped by serde) still
/// fits inside the session fact-value ceiling with room for the row
/// wrapper — arming can therefore never fail on SIZE for a DTO-accepted
/// objective.
pub const MAX_PROMOTION_OBJECTIVE_BYTES: usize = 512;

/// The stable, machine-checkable refusal tag the tool loop answers a
/// [`PROMOTION_REQUEST_TOOL`] call with when the mid-turn promotion itself
/// is refused and deferred to the next submission (the signal
/// `types-refuses with promotion_deferred`). It rides the durable
/// `tool_result` excerpt exactly like every other typed denial tag.
pub const PROMOTION_DEFERRED_TAG: &str = "promotion_deferred";

/// Refusal tag: THIS conversational turn already armed a promotion (one
/// per turn); the earlier objective stands.
pub const PROMOTION_ALREADY_REQUESTED_TAG: &str = "promotion_already_requested";

/// Refusal tag: the call's payload is not the strict
/// [`PromotionRequest`] DTO (missing/empty/oversized objective, wrong
/// type, unknown member). Never arms, never clobbers an armed marker.
pub const PROMOTION_INVALID_TAG: &str = "promotion_invalid";

/// Durable memory-fact kind of the ONE armed pending-promotion marker of a
/// session (a single fixed key — see [`PROMOTION_PENDING_ROW_KEY`], so the
/// bound is one row per session regardless of history length).
pub const PROMOTION_PENDING_ROW_KIND: &str = "turn_promotion";

/// The memory-fact key of the pending-promotion marker. Keyed by a FIXED
/// key — never by op/session — so at most ONE promotion is armed per
/// session at a time and a later arm deterministically replaces an older,
/// never-consumed one (bounded: one row, not a growing list).
pub const PROMOTION_PENDING_ROW_KEY: &str = "pending";

/// Typed rejection of a promotion objective (the strict DTO bound).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionObjectiveError {
    /// Empty (or whitespace-only) objective: a promotion must state what
    /// engineering objective it asks to establish.
    Empty,
    /// Objective exceeds [`MAX_PROMOTION_OBJECTIVE_BYTES`] raw UTF-8 bytes.
    Oversized { bytes: usize, max: usize },
}

impl std::fmt::Display for PromotionObjectiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromotionObjectiveError::Empty => {
                write!(f, "the promotion objective must not be empty")
            }
            PromotionObjectiveError::Oversized { bytes, max } => {
                write!(
                    f,
                    "the promotion objective is {bytes} bytes; the bound is {max}"
                )
            }
        }
    }
}

impl std::error::Error for PromotionObjectiveError {}

/// The ONE strict bound check of a promotion objective: non-empty after
/// trim, at most [`MAX_PROMOTION_OBJECTIVE_BYTES`] raw UTF-8 bytes. Total,
/// deterministic for any valid UTF-8, never locale-sensitive.
pub fn validate_promotion_objective(objective: &str) -> Result<(), PromotionObjectiveError> {
    if objective.trim().is_empty() {
        return Err(PromotionObjectiveError::Empty);
    }
    if objective.len() > MAX_PROMOTION_OBJECTIVE_BYTES {
        return Err(PromotionObjectiveError::Oversized {
            bytes: objective.len(),
            max: MAX_PROMOTION_OBJECTIVE_BYTES,
        });
    }
    Ok(())
}

/// The STRICT tool-call payload of [`PROMOTION_REQUEST_TOOL`]. Required
/// `objective`, no unknown members, bound-checked by
/// [`validate_promotion_objective`] (the JSON-Schema `maxLength` on the
/// offered spec is a character-side hint; THIS decode is the authority).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionRequest {
    /// The engineering objective the conversational turn asks to establish.
    pub objective: String,
}

impl PromotionRequest {
    /// Construct a request whose objective already satisfies the bound.
    pub fn new(objective: impl Into<String>) -> Result<Self, PromotionObjectiveError> {
        let objective = objective.into();
        validate_promotion_objective(&objective)?;
        Ok(Self { objective })
    }

    /// The TRIMMED, bound-checked objective of this request.
    pub fn validated_objective(&self) -> Result<String, PromotionObjectiveError> {
        let trimmed = self.objective.trim();
        validate_promotion_objective(trimmed)?;
        Ok(trimmed.to_string())
    }

    /// Strict decode of one wire/tool-call input value: an OBJECT envelope
    /// only (serde's positional-sequence struct form is refused — the DTO
    /// is strict on the wire shape, not merely on members).
    pub fn from_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "a promotion payload must be a JSON object",
            ));
        }
        serde_json::from_value(value)
    }
}

/// The durable armed pending-promotion marker: the model-stated objective,
/// the signaling TURN's operation id (the one-per-turn bound is checked
/// against it) and the arming clock reading (observability only — the
/// marker is consumed by the next admission, never by a timer). Strict:
/// a present row that does not decode shape-typed (including the objective
/// bound) is a loud typed refusal, never a silent default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnPromotionRow {
    /// The bounded engineering objective the signal stated.
    pub objective: String,
    /// Op id of the conversational turn that armed this marker.
    pub turn_op: u64,
    /// Wall-clock ms at arming (`Clock::now_ms` of the signaling runtime).
    pub armed_ms: i64,
}

impl TurnPromotionRow {
    /// Build a row whose objective is already bound-checked.
    pub fn new(
        objective: impl Into<String>,
        turn_op: u64,
        armed_ms: i64,
    ) -> Result<Self, PromotionObjectiveError> {
        let objective = objective.into();
        validate_promotion_objective(&objective)?;
        Ok(Self {
            objective,
            turn_op,
            armed_ms,
        })
    }

    /// Encode this row as its durable JSON value.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Decode one durable marker value. A malformed/tampered/oversized
    /// value is a typed error — the caller refuses or heals loudly, never
    /// a silent default.
    pub fn decode(value: &str) -> Result<Self, serde_json::Error> {
        let row: Self = serde_json::from_str(value)?;
        validate_promotion_objective(&row.objective).map_err(serde::de::Error::custom)?;
        Ok(row)
    }
}

// ------------------------------------------------------------ session state

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionState {
    pub session_id: String,
    pub state: String,
    pub title: String,
    pub last_event_seq: i64,
    pub agent_state: AgentStateView,
    pub task_ledger: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentStateView {
    pub state: String,
    pub label: String,
    pub active: bool,
    pub terminal: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_behavior_is_explicit() {
        // role may not be null in a message.
        let msg = r#"{"id":"1","role":null,"session_id":"2","seq":0,"created_ms":0,"parts":[]}"#;
        assert!(serde_json::from_str::<Message>(msg).is_err());
    }

    #[test]
    fn message_part_shape_is_strict() {
        let m = Message {
            id: "msg-1".into(),
            role: "assistant".into(),
            session_id: "sess-1".into(),
            seq: 3,
            created_ms: 1700000000000,
            turn_id: Some("op-7".into()),
            parts: vec![
                Part::Text {
                    text: "hello".into(),
                },
                Part::ToolCall {
                    tool_call_id: "call_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                    state: "pending".into(),
                },
                Part::ToolResult {
                    tool_call_id: "call_1".into(),
                    result: ToolResultBody {
                        excerpt: "1 | fn main".into(),
                        exit_code: Some(0),
                        artifact: Some("artifact://abc".into()),
                        slice_hint: Some("artifact://abc?slice=200&len=100".into()),
                    },
                },
            ],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["parts"][1]["type"], "tool_call");
        assert_eq!(v["parts"][2]["type"], "tool_result");
        // Unknown part type rejected.
        let evil = serde_json::json!({"type": "escape_hatch", "text": "x"});
        assert!(serde_json::from_value::<Part>(evil).is_err());
        // Missing `type` rejected.
        assert!(serde_json::from_value::<Part>(serde_json::json!({"text": "x"})).is_err());
        // Tool result without artifact is legal (artifact optional).
        let r = ToolResultBody {
            excerpt: "x".into(),
            exit_code: None,
            artifact: None,
            slice_hint: None,
        };
        assert!(
            serde_json::from_value::<ToolResultBody>(serde_json::to_value(&r).unwrap()).is_ok()
        );
    }

    #[test]
    fn session_state_view_reflects_machine() {
        let v = SessionState {
            session_id: "s1".into(),
            state: "streaming".into(),
            title: "t".into(),
            last_event_seq: 12,
            agent_state: AgentStateView {
                state: "streaming".into(),
                label: "streaming".into(),
                active: true,
                terminal: false,
            },
            task_ledger: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn messages_page_shape_carries_the_bound_and_cursor() {
        let page = MessagesPage {
            session_id: "s1".into(),
            messages: vec![],
            has_more: true,
            next_before: Some(7),
            page: PageMeta {
                size: 100,
                cursor: Some(7),
                has_more: true,
                total_estimate: None,
            },
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["has_more"], true);
        assert_eq!(json["next_before"], 7);
        assert_eq!(json["page"]["size"], 100);
        assert!(json["page"].get("total_estimate").is_none());
        let back: MessagesPage = serde_json::from_value(json).unwrap();
        assert_eq!(back, page);
    }

    /// The execution-kind wire vocabulary is EXPLICIT compatibility: the
    /// default is `task` (older clients change nothing), the two values
    /// round-trip, and an unknown value is a typed decode refusal — never
    /// a silent coercion to either semantics.
    #[test]
    fn execution_kind_defaults_to_task_and_refuses_unknown_values() {
        assert_eq!(ExecutionKind::default(), ExecutionKind::Task);
        assert!(ExecutionKind::Task.is_task());
        assert!(ExecutionKind::Ask.is_ask());
        assert_eq!(ExecutionKind::Task.as_str(), "task");
        assert_eq!(ExecutionKind::Ask.as_str(), "ask");
        assert_eq!(
            serde_json::to_value(ExecutionKind::Ask).unwrap(),
            serde_json::json!("ask")
        );
        assert_eq!(
            serde_json::from_value::<ExecutionKind>(serde_json::json!("task")).unwrap(),
            ExecutionKind::Task
        );
        assert_eq!(
            serde_json::from_value::<ExecutionKind>(serde_json::json!("ask")).unwrap(),
            ExecutionKind::Ask
        );
        for hostile in ["implement", "Task", "ASK", "chat", ""] {
            assert!(
                serde_json::from_value::<ExecutionKind>(serde_json::json!(hostile)).is_err(),
                "{hostile:?} must be a typed refusal"
            );
        }
    }

    /// The turn-execution row is OP-scoped (two turns of one session never
    /// alias) and STRICT: a corrupt/extended value is a loud decode error,
    /// and the legacy shape (kind only) decodes with the empty attachment
    /// set.
    #[test]
    fn turn_execution_row_is_op_scoped_and_strict() {
        assert_ne!(turn_execution_row_key(1), turn_execution_row_key(2));
        assert_eq!(turn_execution_row_key(0x2a), "turn-exec:000000000000002a");
        let row = TurnExecution {
            execution: ExecutionKind::Ask,
            attachments: Vec::new(),
        };
        let encoded = row.encode().unwrap();
        assert!(!encoded.contains("attachments"), "empty set is omitted");
        assert_eq!(TurnExecution::decode(&encoded).unwrap(), row);
        assert_eq!(
            TurnExecution::decode(r#"{"execution":"task"}"#)
                .unwrap()
                .execution,
            ExecutionKind::Task
        );
        assert!(
            TurnExecution::decode(r#"{"execution":"implement"}"#).is_err(),
            "an unknown kind is a loud typed refusal"
        );
        assert!(
            TurnExecution::decode(r#"{"execution":"task","oops":1}"#).is_err(),
            "an extended row is a loud typed refusal"
        );
        assert!(TurnExecution::decode("not json").is_err());
    }

    /// `Message.turn_id` is ADDITIVE wire vocabulary: a legacy message JSON
    /// without the field decodes with `None` (history is never rewritten),
    /// a populated row round-trips, and the field is omitted entirely when
    /// absent so the legacy bytes stay byte-identical.
    #[test]
    fn message_turn_id_is_additive_and_legacy_rows_decode_null() {
        let legacy =
            r#"{"id":"1","role":"user","session_id":"2","seq":1,"created_ms":0,"parts":[]}"#;
        let decoded: Message = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.turn_id, None, "legacy rows fall back to null");
        let encoded = serde_json::to_value(&decoded).unwrap();
        assert!(
            encoded.get("turn_id").is_none(),
            "an absent turn_id is never serialized: {encoded}"
        );
        let with_turn = Message {
            turn_id: Some("000000000000002a".into()),
            ..decoded
        };
        let value = serde_json::to_value(&with_turn).unwrap();
        assert_eq!(value["turn_id"], "000000000000002a");
        let back: Message = serde_json::from_value(value).unwrap();
        assert_eq!(back, with_turn);
    }

    /// The lazy turn-intent row is OP-scoped (two turns of one session never
    /// alias), STRICT (corrupt/extended values are loud refusals), and the
    /// legacy shape (no row) is the documented absence. The verdicts carry
    /// their engineering/conversational meaning losslessly.
    #[test]
    fn turn_intent_row_is_op_scoped_strict_and_lossless() {
        assert_ne!(turn_intent_row_key(1), turn_intent_row_key(2));
        assert_eq!(turn_intent_row_key(0x2a), "turn-intent:000000000000002a");
        for (intent, execution, wire) in [
            (
                TurnIntent::Conversational,
                ExecutionKind::Ask,
                "conversational",
            ),
            (
                TurnIntent::EstablishContract,
                ExecutionKind::Task,
                "establish_contract",
            ),
            (
                TurnIntent::ContinueObjective,
                ExecutionKind::Task,
                "continue_objective",
            ),
        ] {
            assert_eq!(intent.as_str(), wire);
            let row = TurnIntentRow::new(intent, execution);
            let encoded = row.encode().unwrap();
            // The additive promotion field is SERIALIZED AWAY when absent:
            // every legacy row's durable bytes stay byte-identical.
            assert!(
                !encoded.contains("promotion_objective"),
                "legacy bytes changed: {encoded}"
            );
            assert_eq!(TurnIntentRow::decode(&encoded).unwrap(), row);
        }
        assert!(!TurnIntent::Conversational.is_engineering());
        assert!(TurnIntent::EstablishContract.is_engineering());
        assert!(TurnIntent::ContinueObjective.is_engineering());
        for hostile in [
            "not json",
            r#"{"intent":"implement","execution":"task"}"#,
            r#"{"intent":"conversational","execution":"chat"}"#,
            r#"{"intent":"conversational","execution":"ask","oops":1}"#,
            // A promotion objective outside the strict bound is loud too.
            r#"{"intent":"establish_contract","execution":"task","promotion_objective":""}"#,
        ] {
            assert!(
                TurnIntentRow::decode(hostile).is_err(),
                "{hostile:?} must be a typed refusal"
            );
        }
        // A bound-valid promotion objective rides the row losslessly.
        let armed = TurnIntentRow {
            intent: TurnIntent::EstablishContract,
            execution: ExecutionKind::Task,
            promotion_objective: Some("fix the retry parser".into()),
        };
        let encoded = armed.encode().unwrap();
        assert!(encoded.contains("promotion_objective"));
        assert_eq!(TurnIntentRow::decode(&encoded).unwrap(), armed);
    }

    /// The promotion objective bound is the same total, deterministic
    /// byte check everywhere (signal DTO, marker row, intent row): empty
    /// and whitespace-only are refusals, the exact boundary passes, one
    /// byte over fails, and it never depends on char count vs bytes for
    /// multi-byte UTF-8.
    #[test]
    fn promotion_objective_bound_is_total_and_byte_exact() {
        assert_eq!(validate_promotion_objective("fix the parser"), Ok(()));
        assert_eq!(
            validate_promotion_objective(&"x".repeat(MAX_PROMOTION_OBJECTIVE_BYTES)),
            Ok(())
        );
        assert_eq!(
            validate_promotion_objective(""),
            Err(PromotionObjectiveError::Empty)
        );
        assert_eq!(
            validate_promotion_objective("   \t\n "),
            Err(PromotionObjectiveError::Empty)
        );
        let over = "x".repeat(MAX_PROMOTION_OBJECTIVE_BYTES + 1);
        assert_eq!(
            validate_promotion_objective(&over),
            Err(PromotionObjectiveError::Oversized {
                bytes: MAX_PROMOTION_OBJECTIVE_BYTES + 1,
                max: MAX_PROMOTION_OBJECTIVE_BYTES,
            })
        );
        // 171 multi-byte "é" chars are 342 bytes: bytes govern, not chars.
        let accented = "é".repeat(171);
        assert_eq!(accented.chars().count(), 171);
        assert_eq!(validate_promotion_objective(&accented), Ok(()));
        let accented_over = "é".repeat((MAX_PROMOTION_OBJECTIVE_BYTES / 2) + 1);
        assert!(matches!(
            validate_promotion_objective(&accented_over),
            Err(PromotionObjectiveError::Oversized { .. })
        ));
        // The worst-case row encoding of any ACCEPTED objective must fit
        // the session fact-value ceiling (4096): every byte maximally
        // escaped (6-byte \u form) plus the row wrapper.
        let worst = "\u{1}".repeat(MAX_PROMOTION_OBJECTIVE_BYTES);
        let row = TurnPromotionRow::new(&worst, 0x2a, 1).unwrap();
        assert!(
            row.encode().unwrap().len() < 4096,
            "arming can never fit a fact value"
        );
    }

    /// The signal's wire DTO is strict on EVERY axis: required objective,
    /// exact type, no unknown members, the trim+bound check, and a stable
    /// deny_unknown_fields refusal for hostile payloads.
    #[test]
    fn promotion_request_dto_is_strict_on_every_axis() {
        let ok = PromotionRequest::from_value(serde_json::json!({"objective": "fix the parser"}))
            .unwrap();
        assert_eq!(ok.validated_objective().unwrap(), "fix the parser");
        let padded =
            PromotionRequest::from_value(serde_json::json!({"objective": "  trim me  "})).unwrap();
        assert_eq!(padded.validated_objective().unwrap(), "trim me");
        for hostile in [
            serde_json::json!({}),
            serde_json::json!({"objective": null}),
            serde_json::json!({"objective": 7}),
            serde_json::json!({"objective": ["x"]}),
            serde_json::json!({"objective": "ok", "scope": "workspace"}),
            serde_json::json!({"objective": "x", "goal": "y", "oops": 1}),
            serde_json::Value::String("just a string".to_string()),
            serde_json::json!(["objective"]),
            serde_json::Value::Null,
            serde_json::json!(7),
        ] {
            assert!(
                PromotionRequest::from_value(hostile.clone()).is_err(),
                "{hostile:?} must be a typed DTO refusal"
            );
        }
        // Bound refusals: shape-valid strings the validator rejects (the
        // interceptor answers the same typed tag either way).
        for bound_hostile in [
            serde_json::json!({"objective": ""}),
            serde_json::json!({"objective": "   "}),
            serde_json::json!({"objective": "\u{9}\u{a}\u{d} "}),
            serde_json::json!({"objective": "x".repeat(MAX_PROMOTION_OBJECTIVE_BYTES + 1)}),
        ] {
            let request = PromotionRequest::from_value(bound_hostile.clone())
                .expect("shape is valid; the bound governs");
            assert!(
                request.validated_objective().is_err(),
                "{bound_hostile:?} must be refused by the objective bound"
            );
        }
        // The objective bound is enforced by construction too.
        assert!(PromotionRequest::new("").is_err());
        assert!(PromotionRequest::new("x".repeat(MAX_PROMOTION_OBJECTIVE_BYTES + 1)).is_err());
        assert!(PromotionRequest::new("fix the parser").is_ok());
    }

    /// The armed marker row is strict + bound-checked, its one-per-turn
    /// identity (turn_op) round-trips losslessly, and the reserved names
    /// of the signal vocabulary are exactly the documented stable tags —
    /// never renumbered, never renamed.
    #[test]
    fn turn_promotion_row_is_strict_bound_checked_and_identity_lossless() {
        assert_eq!(PROMOTION_REQUEST_TOOL, "promotion_request");
        assert_eq!(PROMOTION_DEFERRED_TAG, "promotion_deferred");
        assert_eq!(
            PROMOTION_ALREADY_REQUESTED_TAG,
            "promotion_already_requested"
        );
        assert_eq!(PROMOTION_INVALID_TAG, "promotion_invalid");
        assert_eq!(PROMOTION_PENDING_ROW_KIND, "turn_promotion");
        assert_eq!(PROMOTION_PENDING_ROW_KEY, "pending");
        assert!(
            PROMOTION_PENDING_ROW_KIND.len() <= 64,
            "fact kinds are bounded"
        );

        let row = TurnPromotionRow::new("fix the retry parser", 0x2a, 1_700_000_000_000).unwrap();
        let encoded = row.encode().unwrap();
        assert_eq!(TurnPromotionRow::decode(&encoded).unwrap(), row);
        for hostile in [
            "not json",
            r#"{"objective":"x","turn_op":"0x2a","armed_ms":1}"#,
            r#"{"objective":"","turn_op":1,"armed_ms":1}"#,
            r#"{"objective":"x","turn_op":1}"#,
            r#"{"objective":"x","turn_op":1,"armed_ms":1,"oops":1}"#,
        ] {
            assert!(
                TurnPromotionRow::decode(hostile).is_err(),
                "{hostile:?} must be a typed refusal"
            );
        }
        let oversized = format!(
            r#"{{"objective":"{}","turn_op":1,"armed_ms":1}}"#,
            "x".repeat(MAX_PROMOTION_OBJECTIVE_BYTES + 1)
        );
        assert!(TurnPromotionRow::decode(&oversized).is_err());
    }
}
