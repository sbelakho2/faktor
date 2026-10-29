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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnIntentRow {
    pub intent: TurnIntent,
    pub execution: ExecutionKind,
}

impl TurnIntentRow {
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
            let row = TurnIntentRow { intent, execution };
            let encoded = row.encode().unwrap();
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
        ] {
            assert!(
                TurnIntentRow::decode(hostile).is_err(),
                "{hostile:?} must be a typed refusal"
            );
        }
    }
}
