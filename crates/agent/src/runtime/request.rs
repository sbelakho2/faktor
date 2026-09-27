//! `runtime::request`: cohesive slice of the agent runtime.

#![allow(unused_imports)]

use super::*;

/// History retrieval bound (audit 29: the conversation window is chosen by
/// the budget-aware planner BEFORE loading — greedy newest-first until the
/// message cap or the byte bound is hit; the store never reads rows beyond
/// the returned window. Token trimming inside the WirePlan remains the
/// exact final authority, but history is never loaded wholesale just to be
/// trimmed afterward).
pub(crate) const MAX_HISTORY_MESSAGES: usize = 2000;

/// Byte-per-token conversion for the load bound (audit 29): the context
/// estimator never rates dense ASCII text below ~3 bytes/token (chars/3 + 1
/// envelope over 1 byte/char), so `max_bytes = tokens * 3` is a
/// conservative token proxy for the row payloads the loader can see.
/// Multi-byte text only makes the bound MORE conservative (more bytes per
/// token), so the loader can never materially over-read relative to the
/// planner's token budget.
pub(crate) const HISTORY_BYTES_PER_TOKEN: u64 = 3;

/// Token floor for the load bound: even a pathologically small budget must
/// still admit the newest ~2 turns (~800 tokens ≈ 2400 payload bytes of
/// dense ASCII) so the provider is never starved of conversation context.
pub(crate) const HISTORY_TOKEN_FLOOR: usize = 800;

/// How the runtime asks for permission. The server implementation waits on a
/// durable permission row + a UI response channel (async so blocking on the
/// user never stalls a tokio worker).
pub trait PermissionRequester: Send + Sync {
    fn request(
        &self,
        session: SessionId,
        permission: &SessionPermission,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
    >;
}

/// Lightweight row view used by history loading (parts are fetched per row
/// by the callers).
pub(crate) struct MessageRowLike {
    pub(crate) id: i64,
    #[allow(dead_code)]
    pub(crate) seq: i64,
    pub(crate) role: String,
    pub(crate) data: serde_json::Value,
}

/// Convert the compactor's kept text turns back into provider messages: the
/// compacted history that rides the next wire request. Text-only by
/// construction (compaction works on `RecentTurn`, which carries text).
pub(crate) fn recent_turns_to_messages(turns: &[RecentTurn]) -> Vec<RequestMessage> {
    turns
        .iter()
        .map(|t| RequestMessage {
            role: if t.role == "user" {
                Role::User
            } else {
                Role::Assistant
            },
            content: vec![ContentPart::text(&t.text)],
        })
        .collect()
}

impl AgentRuntime {
    /// Deterministic conversation-window bounds for one logical turn
    /// (audit 29). The window is sized from the budget class reserved for
    /// recent conversation (spec §8 class 4) with a token floor for tiny
    /// budgets: `(max_messages, max_bytes)` feeds
    /// `messages_backwards_bounded`, which reads exactly the newest window
    /// and never materializes older rows.
    pub(crate) fn history_window_bounds(budget: &ContextBudget) -> (u64, u64) {
        let token_budget = budget.recent.max(HISTORY_TOKEN_FLOOR) as u64;
        (
            MAX_HISTORY_MESSAGES as u64,
            token_budget.saturating_mul(HISTORY_BYTES_PER_TOKEN),
        )
    }

    /// Load the durable history rows (oldest first) for one logical turn.
    /// The window is chosen by the budget-aware planner BEFORE any load
    /// (audit 29): `messages_backwards_bounded` walks the store's backward
    /// index newest-first and stops at the message cap or the byte bound —
    /// rows the planner would trim are never read at all. The 40-message
    /// hard limit is long gone (audit round 6); the WirePlan still does the
    /// exact token-based trimming over the bounded window. The read itself
    /// runs on the session manager's bounded read pool (audit 13): the
    /// SQLite I/O + JSON decode never block a Tokio worker.
    pub(crate) async fn load_history_rows(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
    ) -> faktor_core::Result<Vec<MessageRowLike>> {
        let (max_messages, max_bytes) = Self::history_window_bounds(budget);
        let mut collected: Vec<MessageRowLike> = self
            .deps
            .session
            .messages_backwards_bounded(handle.id(), None, max_messages, max_bytes)
            .await?
            .into_iter()
            .map(|row| MessageRowLike {
                id: row.id,
                seq: row.seq,
                role: row.role.clone(),
                data: row.data.clone(),
            })
            .collect();
        collected.reverse(); // oldest first
        Ok(collected)
    }

    pub(crate) async fn recent_turns(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
    ) -> faktor_core::Result<Vec<RecentTurn>> {
        let rows = self.load_history_rows(handle, budget).await?; // oldest-first
        let mut turns = Vec::new();
        for row in rows {
            let mut pushed_text = false;
            for part in handle.parts_of(row.id)? {
                if part.kind == "text" {
                    if let Some(text) = part.data.get("text").and_then(|v| v.as_str()) {
                        turns.push(RecentTurn {
                            role: row.role.clone(),
                            text: text.to_string(),
                        });
                        pushed_text = true;
                    }
                }
            }
            // The durable user prompt lives in the message payload
            // (submit_prompt stores `{"text": ...}` with no part rows): it
            // must reach the wire and the compactor too.
            if row.role == "user" && !pushed_text {
                if let Some(text) = row.data.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        turns.push(RecentTurn {
                            role: "user".into(),
                            text: text.to_string(),
                        });
                    }
                }
            }
        }
        Ok(turns)
    }

    /// Reconstruct the full provider message list from the durable state,
    /// oldest first. The persisted part order is the source of truth:
    /// text/reasoning/tool calls keep the assistant role, tool results move
    /// to the user role (provider APIs require tool results to come from the
    /// user), and a message carrying only tool results yields one user-role
    /// request message. The durable user prompt (message payload `{"text":
    /// ...}`, no part rows) is synthesized as a user text part — without it
    /// the model would never see the prompt.
    pub(crate) async fn history_messages(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
    ) -> faktor_core::Result<Vec<RequestMessage>> {
        let rows = self.load_history_rows(handle, budget).await?; // oldest-first
        let mut out = Vec::new();
        for row in rows {
            let role_is_user = row.role == "user";
            let mut user_parts: Vec<ContentPart> = Vec::new();
            let mut assistant_parts: Vec<ContentPart> = Vec::new();
            let mut had_text_part = false;
            for part in handle.parts_of(row.id)? {
                match part.kind.as_str() {
                    "text" => {
                        had_text_part = true;
                        let text = str_field(&part.data, "text")?;
                        if role_is_user {
                            user_parts.push(ContentPart::text(text));
                        } else {
                            assistant_parts.push(ContentPart::text(text));
                        }
                    }
                    "reasoning" => {
                        let text = str_field(&part.data, "text")?;
                        if role_is_user {
                            user_parts.push(ContentPart::reasoning(text));
                        } else {
                            assistant_parts.push(ContentPart::reasoning(text));
                        }
                    }
                    "tool_call" => {
                        let state = str_field(&part.data, "state")?;
                        if matches!(state.as_str(), "completed" | "error") {
                            assistant_parts.push(ContentPart::tool_call(
                                str_field(&part.data, "tool_call_id")?,
                                str_field(&part.data, "name")?,
                                part.data
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null),
                            ));
                        }
                    }
                    "tool_result" => {
                        let is_error = part
                            .data
                            .get("exit_code")
                            .and_then(|v| if v.is_null() { None } else { v.as_i64() })
                            .is_some_and(|c| c != 0);
                        user_parts.push(ContentPart::tool_result(
                            str_field(&part.data, "excerpt")?,
                            is_error,
                            str_field(&part.data, "tool_call_id")?,
                        ));
                    }
                    "summary" => {}
                    other => {
                        return Err(Error::malformed(format!(
                            "corrupt durable part kind {other:?} on message {}",
                            row.id
                        )));
                    }
                }
            }
            // Message-level payload: the durable user prompt has no part rows.
            if role_is_user && !had_text_part && user_parts.is_empty() {
                if let Some(text) = row.data.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        user_parts.push(ContentPart::text(text));
                    }
                }
            }
            if role_is_user {
                if !user_parts.is_empty() {
                    out.push(RequestMessage {
                        role: Role::User,
                        content: user_parts,
                    });
                }
            } else {
                if !assistant_parts.is_empty() {
                    out.push(RequestMessage {
                        role: Role::Assistant,
                        content: assistant_parts,
                    });
                }
                if !user_parts.is_empty() {
                    out.push(RequestMessage {
                        role: Role::User,
                        content: user_parts,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Thin adapter: the wire request IS the budgeted plan — `system`,
    /// `messages`, and `tools` each appear exactly once, already measured
    /// against the model budget by the planner.
    pub(crate) fn build_request(
        &self,
        handle: &faktor_session::SessionHandle,
        plan: &WirePlan,
        op_id: OpId,
        model: &str,
        cancel: &CancellationToken,
        attempt: u32,
    ) -> faktor_core::Result<GenericAgentRequest> {
        Ok(GenericAgentRequest {
            model: model.to_string(),
            system: plan.system.clone(),
            messages: plan.messages.clone(),
            tools: plan.tools.clone(),
            max_output: None,
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: op_id,
                session_id: handle.id(),
                provider: handle.provider()?,
                attempt,
                deadline_ms: self.deps.tool_deadline_ms,
                cancellation: cancel.child(),
            },
        })
    }
}
