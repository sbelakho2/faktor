//! Faktor-native protocol types (`docs/native-protocol.md`).
//!
//! These are the daemon's OWN shapes: the conversation view (messages,
//! parts), cursor paging metadata and the session state projection shared
//! by the session layer, the ACP adapter and the HTTP surface. They are
//! Faktor-owned strict structures — never a frozen foreign wire contract.

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
}
