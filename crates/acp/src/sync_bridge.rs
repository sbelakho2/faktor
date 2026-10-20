//! Sync-backend transcript projection: the synchronous ACP engine cannot
//! stream, so its `_meta` final text is projected as the same official
//! `session/update` chunks a streaming backend emits.

use serde_json::{json, Value};

use crate::{notification_body_bytes, session_update_params, MAX_RESPONSE_BYTES};

/// Emit final sync-backend transcript text as official `session/update`
/// chunk notifications. `_meta.agentText` and `_meta.agentReasoning` are the
/// documented sync-backend projection; absent fields emit nothing (a backend
/// that streamed through another channel is never duplicated).
pub(crate) async fn emit_sync_meta_chunks(
    main_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    session_id: &str,
    value: &Value,
) {
    const CHUNK_BYTES: usize = 16 * 1024;
    let Some(meta) = value.get("_meta").and_then(Value::as_object) else {
        return;
    };
    for (field, update) in [
        (
            "agentReasoning",
            agent_thought_chunk_update as fn(&str) -> Value,
        ),
        ("agentText", text_chunk_update as fn(&str) -> Value),
    ] {
        let Some(text) = meta.get(field).and_then(Value::as_str) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + CHUNK_BYTES).min(text.len());
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                break;
            }
            let params = session_update_params(session_id, update(&text[start..end]));
            let frame = notification_body_bytes("session/update", &params);
            if frame.len() > MAX_RESPONSE_BYTES {
                tracing::warn!(
                    session_id,
                    bytes = frame.len(),
                    "acp: sync transcript chunk exceeds the frame bound; stopping transcript projection"
                );
                return;
            }
            if main_tx.send(frame).await.is_err() {
                return;
            }
            start = end;
        }
    }
}

/// `session/update` frame body: official `agent_message_chunk` content
/// frame (`{sessionUpdate, content: {type: "text", text}}`).
pub fn text_chunk_update(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text },
    })
}

/// `session/update` frame body: official `user_message_chunk` (used when
/// replaying `session/load` history; live turns never echo user input).
pub fn user_message_chunk_update(text: &str) -> Value {
    json!({
        "sessionUpdate": "user_message_chunk",
        "content": { "type": "text", "text": text },
    })
}

/// `session/update` frame body: official `agent_thought_chunk`. Reused for
/// native reasoning/summary/system parts (the official schema has no
/// separate kind for those; see [`history_updates`]).
pub fn agent_thought_chunk_update(text: &str) -> Value {
    json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": { "type": "text", "text": text },
    })
}
