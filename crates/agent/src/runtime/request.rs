//! `runtime::request`: cohesive slice of the agent runtime.

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

/// Hard bound on ONE historical tool-call's argument JSON re-sent on the
/// wire. The durable transcript keeps the exact arguments (recovery and UI
/// need them); the model's HISTORY only needs the call identity, its small
/// discriminator fields, and a digest of any large payload it already
/// produced. A `write_file` of a 200 KiB file must not be re-sent on every
/// later turn.
pub(crate) const MAX_WIRE_TOOL_CALL_ARG_BYTES: usize = 2 * 1024;
/// Longest individual string value kept verbatim inside a bounded historical
/// tool-call argument. Anything longer is replaced by a digest marker.
pub(crate) const MAX_WIRE_TOOL_CALL_STRING_BYTES: usize = 256;

/// Minimum excerpt length considered for duplicate-result compaction. A
/// shorter repeated result costs less than the reference marker.
pub(crate) const MIN_DUPLICATE_TOOL_RESULT_BYTES: usize = 256;

/// Hard bound on a durable context-checkpoint payload accepted for replay.
/// A legitimate 1M-token compacted context serializes to a few MiB; 32 MiB
/// is generous headroom while keeping a corrupt/hostile row from forcing an
/// unbounded JSON allocation at session open.
pub(crate) const MAX_CONTEXT_CHECKPOINT_BYTES: usize = 32 * 1024 * 1024;

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

/// Lightweight row view used by history loading. Parts are loaded in ONE
/// bounded batch by the store query, so the caller never runs an N+1
/// `parts_of` walk over the window.
pub(crate) struct MessageRowLike {
    pub(crate) id: i64,
    pub(crate) seq: i64,
    pub(crate) role: String,
    pub(crate) data: serde_json::Value,
    pub(crate) parts: Vec<faktor_store::PartRow>,
}

/// Convert the compactor's kept text turns back into provider messages: the
/// compacted history that rides the next wire request. Roles are preserved:
/// synthesized identity/facts turns stay SYSTEM (never demoted to
/// assistant), prior summaries stay assistant, and tool-role residue is a
/// user-side observation.
pub(crate) fn recent_turns_to_messages(turns: &[RecentTurn]) -> Vec<RequestMessage> {
    turns
        .iter()
        .map(|t| RequestMessage {
            role: match t.role.as_str() {
                "user" | "tool" => Role::User,
                "system" => Role::System,
                _ => Role::Assistant,
            },
            content: vec![ContentPart::text(&t.text)],
        })
        .collect()
}

/// Nothing may ride a provider request as a DANGLING tool pairing: a tool
/// result whose call is not in the retained history (a crash between the
/// call append and the result append, or a checkpoint that summarized the
/// call) is dropped, and a call whose result never arrived is dropped too.
/// The durable transcript still holds both; only the WIRE is repaired. The
/// wire planner's boundary trim handles windows that start on a result;
/// this pass covers mid-history orphans the boundary rule cannot see.
pub(crate) fn sanitize_tool_pairing(messages: Vec<RequestMessage>) -> Vec<RequestMessage> {
    let mut call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in &messages {
        for part in &message.content {
            match &part.kind {
                ContentKind::ToolCall { id, .. } => {
                    call_ids.insert(id.clone());
                }
                ContentKind::ToolResult { .. } => {
                    if let Some(id) = part.tool_call_id.as_deref() {
                        result_ids.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    messages
        .into_iter()
        .filter_map(|mut message| {
            message.content.retain(|part| match &part.kind {
                ContentKind::ToolCall { id, .. } => result_ids.contains(id),
                ContentKind::ToolResult { .. } => part
                    .tool_call_id
                    .as_deref()
                    .is_some_and(|id| call_ids.contains(id)),
                _ => true,
            });
            (!message.content.is_empty()).then_some(message)
        })
        .collect()
}

/// Decode the durable context checkpoint's opaque payload into the exact
/// compacted turns. A malformed payload is loud; the caller logs and falls
/// back to the full transcript rather than silently changing context.
pub(crate) fn decode_checkpoint_turns(
    checkpoint: &faktor_session::ContextCheckpoint,
) -> faktor_core::Result<Vec<RecentTurn>> {
    if checkpoint.payload.len() > MAX_CONTEXT_CHECKPOINT_BYTES {
        return Err(Error::oversized(format!(
            "context checkpoint payload of {} bytes exceeds MAX_CONTEXT_CHECKPOINT_BYTES",
            checkpoint.payload.len()
        )));
    }
    serde_json::from_slice(&checkpoint.payload)
        .map_err(|e| Error::malformed(format!("context checkpoint turns: {e}")))
}

/// Bound ONE historical tool-call argument value for the wire.
///
/// Small values are returned BYTE-IDENTICAL (the common case: paths,
/// commands, flags). When the serialized value exceeds
/// [`MAX_WIRE_TOOL_CALL_ARG_BYTES`], long strings are replaced by a
/// deterministic digest marker (first 256 bytes + original length + BLAKE3);
/// if the structural skeleton still exceeds the bound, the whole value is
/// replaced by one bounded digest marker. The projection is total and
/// deterministic, so an unchanged durable call renders the same wire bytes
/// on every turn (prompt-cache friendly) and a hostile 16 MiB argument cannot
/// grow the request.
pub(crate) fn bound_tool_call_input(input: &serde_json::Value) -> serde_json::Value {
    let encoded = serde_json::to_vec(input).unwrap_or_default();
    if encoded.len() <= MAX_WIRE_TOOL_CALL_ARG_BYTES {
        return input.clone();
    }
    let projected = truncate_argument_strings(input, 0);
    let projected_len = serde_json::to_vec(&projected)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if projected_len <= MAX_WIRE_TOOL_CALL_ARG_BYTES {
        projected
    } else {
        serde_json::json!({
            "_bounded": format!(
                "tool arguments elided on the wire ({} bytes, blake3 {})",
                encoded.len(),
                blake3::hash(&encoded).to_hex()
            )
        })
    }
}

fn truncate_argument_strings(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    // Depth bound: a hostile nested argument cannot recurse without a
    // ceiling (JSON depth is already bounded by the parser, but this keeps
    // the projection's own allocation bounded too).
    if depth >= 16 {
        return serde_json::json!("[nested argument elided]");
    }
    match value {
        serde_json::Value::String(text) => {
            if text.len() <= MAX_WIRE_TOOL_CALL_STRING_BYTES {
                value.clone()
            } else {
                let mut end = MAX_WIRE_TOOL_CALL_STRING_BYTES;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                serde_json::Value::String(format!(
                    "{}…[bounded {} bytes, blake3 {}]",
                    &text[..end],
                    text.len(),
                    blake3::hash(text.as_bytes()).to_hex()
                ))
            }
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| truncate_argument_strings(item, depth + 1))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, item) in map {
                let key = if key.len() <= MAX_WIRE_TOOL_CALL_STRING_BYTES {
                    key.clone()
                } else {
                    format!(
                        "key…[bounded {} bytes, blake3 {}]",
                        key.len(),
                        blake3::hash(key.as_bytes()).to_hex()
                    )
                };
                out.insert(key, truncate_argument_strings(item, depth + 1));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

impl AgentRuntime {
    /// Deterministic conversation-window bounds for one logical turn
    /// (audit 29). The window is sized from the budget class reserved for
    /// recent conversation (spec §8 class 4) with a token floor for tiny
    /// budgets: `(max_messages, max_bytes)` feeds
    /// `messages_backwards_bounded`, which reads exactly the newest window
    /// and never materializes older rows.
    pub(crate) fn history_window_bounds(budget: &ContextBudget) -> (u64, u64) {
        // The history class budget — NOT the whole context — is the load
        // bound. The planner can spend `context_max`, so loading to the brim
        // would make every turn sit at ~100% usage and re-trigger proactive
        // compaction forever; keeping the nominal recent-class headroom is
        // what makes the 65% trigger meaningful. The bound is in BYTES and
        // charges parts too ([`Store::messages_window_with_parts`]), so it is
        // the real materialized history, not just the message envelopes.
        let token_budget = budget.recent.max(HISTORY_TOKEN_FLOOR) as u64;
        (
            MAX_HISTORY_MESSAGES as u64,
            token_budget.saturating_mul(HISTORY_BYTES_PER_TOKEN),
        )
    }

    /// The durable context checkpoint, decoded. A corrupt checkpoint is
    /// logged and treated as ABSENT (the full durable transcript is then
    /// replayed): the session must open, but corruption is never silently
    /// mistaken for an honest compacted history.
    pub(crate) fn context_checkpoint_turns(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> (Vec<RecentTurn>, Option<u64>) {
        match handle.context_checkpoint() {
            Ok(Some(checkpoint)) => match decode_checkpoint_turns(&checkpoint) {
                Ok(turns) => (turns, Some(checkpoint.through_seq)),
                Err(error) => {
                    tracing::error!(
                        session = %handle.id(),
                        %error,
                        "durable context checkpoint is malformed; falling back to the full transcript"
                    );
                    (Vec::new(), None)
                }
            },
            Ok(None) => (Vec::new(), None),
            Err(error) => {
                tracing::error!(
                    session = %handle.id(),
                    %error,
                    "durable context checkpoint is unreadable; falling back to the full transcript"
                );
                (Vec::new(), None)
            }
        }
    }

    /// Load the durable history rows (oldest first) for one logical turn,
    /// starting STRICTLY after the durable context checkpoint's high-water
    /// seq. The window is chosen BEFORE any load: the store walks its
    /// backward index newest-first and stops at the byte/message bound, so
    /// rows the planner would trim are never read. The bound charges message
    /// AND part bytes and returns parts in one batch, so neither a long
    /// session nor a table of huge tool text can make one turn load more
    /// than its budget (or run N+1 queries). The read runs on the session
    /// manager's bounded pool, never a Tokio worker.
    pub(crate) async fn load_history_rows(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
        after_seq: Option<u64>,
    ) -> faktor_core::Result<Vec<MessageRowLike>> {
        let (max_messages, max_bytes) = Self::history_window_bounds(budget);
        let mut collected: Vec<MessageRowLike> = self
            .deps
            .session
            .messages_window_with_parts(handle.id(), None, after_seq, max_messages, max_bytes)
            .await?
            .into_iter()
            .map(|(row, parts)| MessageRowLike {
                id: row.id,
                seq: row.seq,
                role: row.role.clone(),
                data: row.data.clone(),
                parts,
            })
            .collect();
        collected.reverse(); // oldest first
        Ok(collected)
    }

    /// The compaction/history view PLUS the source high-water seq the loaded
    /// window covers. The runtime records the accepted plan's replacement
    /// context against this seq, so a restart resumes from the checkpoint
    /// instead of replaying summarized rows.
    pub(crate) async fn recent_turns_with_head(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
    ) -> faktor_core::Result<(Vec<RecentTurn>, Option<u64>)> {
        let (mut turns, checkpoint_seq) = self.context_checkpoint_turns(handle);
        let rows = self
            .load_history_rows(handle, budget, checkpoint_seq)
            .await?; // oldest-first
        let head = rows
            .last()
            .map(|row| u64::try_from(row.seq).unwrap_or(0))
            .or(checkpoint_seq)
            .filter(|seq| *seq > 0);
        for row in rows {
            let mut pushed_text = false;
            for part in &row.parts {
                match part.kind.as_str() {
                    "text" => {
                        if let Some(text) = part.data.get("text").and_then(|v| v.as_str()) {
                            turns.push(RecentTurn {
                                role: row.role.clone(),
                                text: text.to_string(),
                            });
                            pushed_text = true;
                        }
                    }
                    // Tool activity is part of the conversation the
                    // summarizer must see. The durable excerpt is already
                    // bounded (2 KiB); the call rendering is bounded here.
                    // These are TEXT turns, never provider tool parts, so
                    // compaction can never orphan a tool result.
                    "tool_result" => {
                        if let Some(excerpt) = part.data.get("excerpt").and_then(|v| v.as_str()) {
                            let exit = part
                                .data
                                .get("exit_code")
                                .and_then(|v| v.as_i64())
                                .map(|c| format!(" (exit {c})"))
                                .unwrap_or_default();
                            turns.push(RecentTurn {
                                role: "tool".into(),
                                text: format!("{excerpt}{exit}"),
                            });
                        }
                    }
                    "tool_call" => {
                        let name = part
                            .data
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool");
                        let input = part
                            .data
                            .get("input")
                            .map(bound_tool_call_input)
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        turns.push(RecentTurn {
                            role: "tool_call".into(),
                            text: truncate(&format!("{name}({input})"), 2_000),
                        });
                    }
                    _ => {}
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
        Ok((turns, head))
    }

    #[allow(dead_code)] // test/compatibility view; production uses the head-carrying twin
    pub(crate) async fn recent_turns(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
    ) -> faktor_core::Result<Vec<RecentTurn>> {
        Ok(self.recent_turns_with_head(handle, budget).await?.0)
    }

    /// Reconstruct the provider message list from the durable state, oldest
    /// first: the durable context checkpoint (the exact replacement history
    /// of the last accepted compaction) followed by rows newer than its
    /// high-water seq. The persisted part order is the source of truth:
    /// text/reasoning/tool calls keep the assistant role, tool results move
    /// to the user role (provider APIs require tool results to come from the
    /// user), and a message carrying only tool results yields one user-role
    /// request message. The durable user prompt (message payload `{"text":
    /// ...}`, no part rows) is synthesized as a user text part — without it
    /// the model would never see the prompt.
    pub(crate) async fn history_messages_with_head(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
    ) -> faktor_core::Result<(Vec<RequestMessage>, Option<u64>)> {
        let (checkpoint_turns, checkpoint_seq) = self.context_checkpoint_turns(handle);
        let rows = self
            .load_history_rows(handle, budget, checkpoint_seq)
            .await?; // oldest-first
        let head = rows
            .last()
            .map(|row| u64::try_from(row.seq).unwrap_or(0))
            .or(checkpoint_seq)
            .filter(|seq| *seq > 0);
        let mut out = recent_turns_to_messages(&checkpoint_turns);
        // Exact repeated tool results are conversation noise: the model
        // already has the earlier bytes in THIS window. Render the later
        // result as a bounded reference to the first call, keeping the
        // call/result pairing (the provider still sees an answer for every
        // call). Only results long enough to be worth the marker are
        // considered; a shorter repeat stays byte-identical.
        let mut seen_tool_results: std::collections::HashMap<FileHash, String> =
            std::collections::HashMap::new();
        for row in rows {
            let role_is_user = row.role == "user";
            let mut user_parts: Vec<ContentPart> = Vec::new();
            let mut assistant_parts: Vec<ContentPart> = Vec::new();
            let mut had_text_part = false;
            for part in &row.parts {
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
                                bound_tool_call_input(
                                    part.data.get("input").unwrap_or(&serde_json::Value::Null),
                                ),
                            ));
                        }
                    }
                    "tool_result" => {
                        let is_error = part
                            .data
                            .get("exit_code")
                            .and_then(|v| if v.is_null() { None } else { v.as_i64() })
                            .is_some_and(|c| c != 0);
                        let call_id = str_field(&part.data, "tool_call_id")?;
                        let excerpt = str_field(&part.data, "excerpt")?;
                        let excerpt = if excerpt.len() >= MIN_DUPLICATE_TOOL_RESULT_BYTES {
                            let digest =
                                FileHash::from(*blake3::hash(excerpt.as_bytes()).as_bytes());
                            match seen_tool_results.get(&digest) {
                                Some(first_call) => format!(
                                    "[duplicate tool result: identical to call {first_call}; {} bytes]",
                                    excerpt.len()
                                ),
                                None => {
                                    seen_tool_results.insert(digest, call_id.clone());
                                    excerpt
                                }
                            }
                        } else {
                            excerpt
                        };
                        user_parts.push(ContentPart::tool_result(excerpt, is_error, call_id));
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
        Ok((sanitize_tool_pairing(out), head))
    }

    #[allow(dead_code)] // test/compatibility view; production uses the head-carrying twin
    pub(crate) async fn history_messages(
        &self,
        handle: &faktor_session::SessionHandle,
        budget: &ContextBudget,
    ) -> faktor_core::Result<Vec<RequestMessage>> {
        Ok(self.history_messages_with_head(handle, budget).await?.0)
    }

    /// Thin adapter: the wire request IS the budgeted plan — `system`,
    /// `messages`, and `tools` each appear exactly once, already measured
    /// against the model budget by the planner.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_request(
        &self,
        handle: &faktor_session::SessionHandle,
        plan: &WirePlan,
        op_id: OpId,
        model: &str,
        cancel: &CancellationToken,
        attempt: u32,
        max_output: usize,
    ) -> faktor_core::Result<GenericAgentRequest> {
        Ok(GenericAgentRequest {
            model: model.to_string(),
            system: plan.system.clone(),
            messages: plan.messages.clone(),
            tools: plan.tools.clone(),
            // Bounded everything: the wire carries an EXPLICIT output cap.
            // Leaving it None delegates the bound to the provider, and a
            // native Ollama runtime treats an absent num_predict as
            // "generate until stop/context" — a real measured run emitted
            // 20,968 tokens in one iteration. The cap is the resolved
            // model profile's max_output (the same dimension the router
            // priced and qualified this call against).
            max_output: Some(max_output.max(1)),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: op_id,
                session_id: handle.id(),
                provider: handle.provider()?,
                attempt,
                // A model stream is NOT a tool call: the 30 s tool-call
                // deadline killed local generations (and the second of two
                // concurrent children) mid-stream. The model call has its
                // own generous overall bound; provider adapters enforce
                // their stricter first-byte/metadata deadlines, and the
                // turn token still cancels immediately.
                deadline_ms: crate::runtime::MODEL_CALL_DEADLINE_MS,
                cancellation: cancel.child(),
            },
        })
    }
}
