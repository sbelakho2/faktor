//! Durable engineering-goal projection.
//!
//! The user prompt is the authority for the task objective. A prompt that
//! carries a credential must never be discarded wholesale: the durable goal
//! is a SEMANTIC PROJECTION of the prompt with every secret span replaced by
//! [`GOAL_REDACTED_SECRET`]. The original message stays under its normal
//! storage/wire policy; only the derived goal/contract/prompt-prefix surface
//! is redacted.

use faktor_context::ledger::{GoalProjectionState, TaskLedger, GOAL_REDACTED_SECRET};
use faktor_core::error::Error;
use faktor_security::{scan_secrets, SecretPolicy};

use super::*;

/// Bound of the durable projected goal (mirrors the typed ledger's goal
/// bound; the compact prompt copy is rendered from this text).
pub(crate) const MAX_GOAL_PROJECTION_BYTES: usize = 4096;

/// Bound of the operational goal copy (`TaskLedger::goal` / `Task::goal`)
/// carried into the prompt prefix and the compaction projection. The FULL
/// redacted prompt lives in `goal_redacted_text`; the operational copy stays
/// at the historical 200-byte bound so a long prompt cannot inflate the
/// synthesized compaction wire.
pub(crate) const MAX_GOAL_OPERATIONAL_BYTES: usize = 200;

const MAX_REDACTION_PASSES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptGoalProjection {
    pub(crate) source_message_id: i64,
    pub(crate) redacted_text: String,
    pub(crate) digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GoalProjectionOutcome {
    Available(PromptGoalProjection),
    UnavailableAfterRedaction {
        source_message_id: i64,
        redacted_text: String,
        digest: String,
    },
}

/// Project a user prompt into its durable, secret-free goal.
pub(crate) fn project_prompt_goal(source_message_id: i64, prompt: &str) -> GoalProjectionOutcome {
    let redacted = redact_prompt_text(prompt);
    let redaction_removed_content = redacted != prompt;
    let redacted_text = truncate_boundary(&redacted, MAX_GOAL_PROJECTION_BYTES).to_string();
    let digest = projection_digest(source_message_id, &redacted_text);
    if has_meaningful_goal(&redacted_text, redaction_removed_content) {
        GoalProjectionOutcome::Available(PromptGoalProjection {
            source_message_id,
            redacted_text,
            digest,
        })
    } else {
        GoalProjectionOutcome::UnavailableAfterRedaction {
            source_message_id,
            redacted_text,
            digest,
        }
    }
}

/// The redacted text of a prompt/goal, preserving every non-secret byte.
pub(crate) fn redact_prompt_text(text: &str) -> String {
    let policy = SecretPolicy::default();
    let mut current = text.to_string();
    for _ in 0..MAX_REDACTION_PASSES {
        let mut changed = false;
        let spans = secret_spans(&current, &policy);
        if !spans.is_empty() {
            current = replace_spans(&current, &spans);
            changed = true;
        }
        let encoded = redact_encoded_tokens(&current, &policy);
        if encoded != current {
            current = encoded;
            changed = true;
        }
        let wrapped = redact_wrapped_encoded_blocks(&current, &policy);
        if wrapped != current {
            current = wrapped;
            changed = true;
        }
        if !changed {
            break;
        }
    }
    current
}

/// TRUE when the text still carries a detectable secret (including the
/// encoded forms the projection redactor covers).
pub(crate) fn contains_secret(text: &str) -> bool {
    let policy = SecretPolicy::default();
    !secret_spans(text, &policy).is_empty()
        || encoded_secret_token(text, &policy).is_some()
        || wrapped_encoded_blocks(text)
            .iter()
            .any(|(_, _, lines)| block_carries_secret(lines, &policy))
}

/// The durable goal text used by contract compilation and the prompt
/// prefix: the redacted projection, bounded to the ledger's goal bound.
pub(crate) fn durable_goal_text(text: &str) -> String {
    truncate_boundary(&redact_prompt_text(text), MAX_GOAL_PROJECTION_BYTES).to_string()
}

/// Drive-start initialization/migration of the durable goal.
///
/// * empty ledger + prompt -> the redacted projection (never the title);
/// * empty ledger + no prompt -> the title is the only available label;
/// * legacy goal equal to the title (the P1 bug) -> adopt the prompt
///   projection when one exists;
/// * any legacy goal carrying a secret -> scrub it in place (the non-secret
///   objective survives);
/// * a prompt that redacts to nothing meaningful -> typed
///   [`ErrorKind::GoalUnavailableAfterRedaction`], never a "task" goal.
pub(crate) fn initialize_durable_goal(
    handle: &faktor_session::SessionHandle,
    ledger: &mut TaskLedger,
) -> faktor_core::Result<()> {
    let title = handle.title().unwrap_or_default();
    let latest = AgentRuntime::latest_user_prompt_message(handle);
    let mut changed = false;

    if ledger.goal_unavailable_after_redaction() {
        return Err(Error::goal_unavailable_after_redaction(
            ledger.goal_source_message_id.unwrap_or(0),
        ));
    }

    if ledger.goal.is_empty() {
        match latest.as_ref() {
            Some((source_message_id, prompt)) => {
                adopt_prompt_projection(handle, ledger, *source_message_id, prompt)?;
                return Ok(());
            }
            None => {
                ledger.goal = truncate_boundary(&title, MAX_GOAL_PROJECTION_BYTES).to_string();
                if !ledger.goal.is_empty() {
                    changed = true;
                }
            }
        }
    } else if ledger.goal == title && ledger.goal_projection_state.is_none() {
        if let Some((source_message_id, prompt)) = latest.as_ref() {
            match project_prompt_goal(*source_message_id, prompt) {
                GoalProjectionOutcome::Available(projection) => {
                    apply_projection(ledger, &projection);
                    changed = true;
                }
                GoalProjectionOutcome::UnavailableAfterRedaction {
                    source_message_id,
                    redacted_text,
                    digest,
                } => {
                    mark_unavailable(ledger, source_message_id, &redacted_text, &digest);
                    persist_goal(handle, ledger)?;
                    clear_label_task_goal(handle, &title);
                    return Err(Error::goal_unavailable_after_redaction(source_message_id));
                }
            }
        }
    } else {
        let scrubbed = durable_goal_text(&ledger.goal);
        let redaction_removed_content = scrubbed != ledger.goal;
        if !has_meaningful_goal(&scrubbed, redaction_removed_content) {
            let source_message_id = ledger.goal_source_message_id.unwrap_or(0);
            let digest = projection_digest(source_message_id, &scrubbed);
            mark_unavailable(ledger, source_message_id, &scrubbed, &digest);
            persist_goal(handle, ledger)?;
            clear_label_task_goal(handle, &title);
            return Err(Error::goal_unavailable_after_redaction(source_message_id));
        }
        let operational = operational_goal_excerpt(&scrubbed);
        if operational != ledger.goal {
            ledger.goal = operational;
            changed = true;
        }
    }

    changed |= scrub_projection_fields(ledger);
    changed |= fill_legacy_projection(ledger, latest.map(|(id, _)| id));
    if changed {
        persist_goal(handle, ledger)?;
    }
    Ok(())
}

/// The redacted projected goal for pre-drive task-row creation, if the
/// latest prompt has one.
pub(crate) fn projected_goal_text(handle: &faktor_session::SessionHandle) -> Option<String> {
    let (source_message_id, prompt) = AgentRuntime::latest_user_prompt_message(handle)?;
    match project_prompt_goal(source_message_id, &prompt) {
        GoalProjectionOutcome::Available(projection) => {
            Some(operational_goal_excerpt(&projection.redacted_text))
        }
        GoalProjectionOutcome::UnavailableAfterRedaction { .. } => None,
    }
}

fn adopt_prompt_projection(
    handle: &faktor_session::SessionHandle,
    ledger: &mut TaskLedger,
    source_message_id: i64,
    prompt: &str,
) -> faktor_core::Result<()> {
    match project_prompt_goal(source_message_id, prompt) {
        GoalProjectionOutcome::Available(projection) => {
            apply_projection(ledger, &projection);
            persist_goal(handle, ledger)?;
            Ok(())
        }
        GoalProjectionOutcome::UnavailableAfterRedaction {
            source_message_id,
            redacted_text,
            digest,
        } => {
            mark_unavailable(ledger, source_message_id, &redacted_text, &digest);
            persist_goal(handle, ledger)?;
            clear_label_task_goal(handle, &handle.title().unwrap_or_default());
            Err(Error::goal_unavailable_after_redaction(source_message_id))
        }
    }
}

pub(crate) fn apply_projection(ledger: &mut TaskLedger, projection: &PromptGoalProjection) {
    ledger.goal = operational_goal_excerpt(&projection.redacted_text);
    ledger.goal_source_message_id = Some(projection.source_message_id);
    ledger.goal_redacted_text = Some(projection.redacted_text.clone());
    ledger.goal_projection_digest = Some(projection.digest.clone());
    ledger.goal_projection_state = Some(GoalProjectionState::Available);
}

fn mark_unavailable(
    ledger: &mut TaskLedger,
    source_message_id: i64,
    redacted_text: &str,
    digest: &str,
) {
    ledger.goal.clear();
    ledger.goal_source_message_id = Some(source_message_id);
    ledger.goal_redacted_text = Some(redacted_text.to_string());
    ledger.goal_projection_digest = Some(digest.to_string());
    ledger.goal_projection_state = Some(GoalProjectionState::GoalUnavailableAfterRedaction);
}

fn scrub_projection_fields(ledger: &mut TaskLedger) -> bool {
    let mut changed = false;
    if let Some(text) = ledger.goal_redacted_text.clone() {
        let scrubbed = durable_goal_text(&text);
        if scrubbed != text {
            ledger.goal_redacted_text = Some(scrubbed);
            changed = true;
        }
    }
    if let Some(digest) = ledger.goal_projection_digest.as_deref() {
        let source_message_id = ledger.goal_source_message_id.unwrap_or(0);
        let text = ledger
            .goal_redacted_text
            .as_deref()
            .unwrap_or(ledger.goal.as_str());
        let expected = projection_digest(source_message_id, text);
        if digest != expected {
            ledger.goal_projection_digest = Some(expected);
            changed = true;
        }
    }
    changed
}

fn fill_legacy_projection(ledger: &mut TaskLedger, source_message_id: Option<i64>) -> bool {
    if ledger.goal.is_empty() || ledger.goal_unavailable_after_redaction() {
        return false;
    }
    let Some(source_message_id) = ledger.goal_source_message_id.or(source_message_id) else {
        return false;
    };
    let mut changed = false;
    if ledger.goal_redacted_text.is_none() {
        ledger.goal_redacted_text = Some(ledger.goal.clone());
        changed = true;
    }
    if ledger.goal_source_message_id.is_none() {
        ledger.goal_source_message_id = Some(source_message_id);
        changed = true;
    }
    let text = ledger
        .goal_redacted_text
        .as_deref()
        .unwrap_or(ledger.goal.as_str());
    let expected = projection_digest(source_message_id, text);
    if ledger.goal_projection_digest.as_deref() != Some(expected.as_str()) {
        ledger.goal_projection_digest = Some(expected);
        changed = true;
    }
    if ledger.goal_projection_state != Some(GoalProjectionState::Available) {
        ledger.goal_projection_state = Some(GoalProjectionState::Available);
        changed = true;
    }
    changed
}

fn persist_goal(
    handle: &faktor_session::SessionHandle,
    ledger: &TaskLedger,
) -> faktor_core::Result<()> {
    handle.put_task_ledger(serde_json::to_value(ledger)?)?;
    Ok(())
}

fn clear_label_task_goal(handle: &faktor_session::SessionHandle, title: &str) {
    let Ok(task_id) = handle.task_id() else {
        return;
    };
    let Ok(Some(task)) = handle.get_task(task_id) else {
        return;
    };
    if task.state.is_terminal() {
        return;
    }
    if task.goal == title || task.goal.is_empty() || contains_secret(&task.goal) {
        let _ = handle.update_task(
            task.task_id,
            TaskPatch {
                goal: Some(String::new()),
                ..Default::default()
            },
        );
    }
}

fn projection_digest(source_message_id: i64, redacted_text: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-goal-projection:v1\0");
    hasher.update(&source_message_id.to_le_bytes());
    hasher.update(&(redacted_text.len() as u64).to_le_bytes());
    hasher.update(redacted_text.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// The bounded operational goal: the head of the redacted text, or (when
/// the head is only markers/generic words) a window whose first meaningful
/// token is the objective. The FULL redacted text remains in
/// `goal_redacted_text`; this excerpt is what the prompt/compaction carry.
fn operational_goal_excerpt(redacted: &str) -> String {
    let head = trim_partial_marker(truncate_boundary(redacted, MAX_GOAL_OPERATIONAL_BYTES));
    if redacted.len() <= MAX_GOAL_OPERATIONAL_BYTES || has_meaningful_goal(head, true) {
        return head.to_string();
    }
    let Some(start) = first_meaningful_token_offset(redacted) else {
        return head.to_string();
    };
    let mut begin = start.saturating_sub(64);
    while begin > 0 && !redacted.is_char_boundary(begin) {
        begin -= 1;
    }
    truncate_boundary(&redacted[begin..], MAX_GOAL_OPERATIONAL_BYTES).to_string()
}

/// A bounded cut can land inside `[REDACTED_SECRET]`; the fragment must not
/// masquerade as an objective token (`..._SEC`), so cut before the marker.
fn trim_partial_marker(text: &str) -> &str {
    let Some(open) = text.rfind('[') else {
        return text;
    };
    let tail = &text[open..];
    if tail.len() < GOAL_REDACTED_SECRET.len() && GOAL_REDACTED_SECRET.starts_with(tail) {
        &text[..open]
    } else {
        text
    }
}

fn first_meaningful_token_offset(text: &str) -> Option<usize> {
    let mut start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            start.get_or_insert(index);
        } else if let Some(token_start) = start.take() {
            if token_is_meaningful(&text[token_start..index]) {
                return Some(token_start);
            }
        }
    }
    start.filter(|token_start| token_is_meaningful(&text[*token_start..]))
}

fn token_is_meaningful(token: &str) -> bool {
    token.chars().count() >= 3
        && !GENERIC_GOAL_TOKENS.contains(&token.to_ascii_lowercase().as_str())
}

fn has_meaningful_goal(text: &str, redaction_removed_content: bool) -> bool {
    if !redaction_removed_content {
        return text.chars().any(char::is_alphanumeric);
    }
    let without_markers = text.replace(GOAL_REDACTED_SECRET, " ");
    without_markers
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().count() >= 3)
        .any(|token| {
            let lower = token.to_ascii_lowercase();
            !GENERIC_GOAL_TOKENS.contains(&lower.as_str())
        })
}

const GENERIC_GOAL_TOKENS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "this",
    "that",
    "these",
    "those",
    "from",
    "into",
    "here",
    "there",
    "please",
    "use",
    "using",
    "secret",
    "secrets",
    "credential",
    "credentials",
    "token",
    "tokens",
    "key",
    "keys",
    "apikey",
    "password",
    "passwords",
    "pwd",
    "redacted",
    "redacted_secret",
];

fn secret_spans(text: &str, policy: &SecretPolicy) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = scan_secrets(text, policy)
        .into_iter()
        .map(|hit| {
            let end = hit.offset.saturating_add(hit.len);
            if hit.kind == "pem_private_key" {
                expand_pem_span(text, hit.offset, end)
            } else {
                (hit.offset, end.min(text.len()))
            }
        })
        .collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        if start >= end || end > text.len() {
            continue;
        }
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn expand_pem_span(text: &str, start: usize, end: usize) -> (usize, usize) {
    let tail = &text[end..];
    let Some(rel_end_tag) = tail.find("-----END ") else {
        return (start, text.len());
    };
    let after_tag = &tail[rel_end_tag..];
    let Some(rel_type) = after_tag.find("PRIVATE KEY-----") else {
        return (start, text.len());
    };
    let mut stop = end + rel_end_tag + rel_type + "PRIVATE KEY-----".len();
    while let Some(ch) = text[stop..].chars().next() {
        if ch == '\r' || ch == '\n' {
            stop += ch.len_utf8();
        } else {
            break;
        }
    }
    (start, stop)
}

fn replace_spans(text: &str, spans: &[(usize, usize)]) -> String {
    let mut out = String::with_capacity(text.len() + spans.len() * GOAL_REDACTED_SECRET.len());
    let mut cursor = 0;
    for &(start, end) in spans {
        if start < cursor || start > text.len() || end > text.len() {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(GOAL_REDACTED_SECRET);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn redact_encoded_tokens(text: &str, policy: &SecretPolicy) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in token_runs(text) {
        let token = &text[start..end];
        let (core_start, core_end) = strip_edge_punctuation(token);
        if core_start >= core_end {
            continue;
        }
        if !encoded_token_has_secret(&token[core_start..core_end], policy) {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(&token[..core_start]);
        out.push_str(GOAL_REDACTED_SECRET);
        out.push_str(&token[core_end..]);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Line-wrapped encodings: a base64/hex secret split over several lines is
/// still the same credential. Consecutive encoded-looking lines form one
/// block; the block is decoded as a single token and replaced when it
/// carries a secret. Separators inside the block are redacted with it.
fn redact_wrapped_encoded_blocks(text: &str, policy: &SecretPolicy) -> String {
    let blocks = wrapped_encoded_blocks(text);
    if blocks.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end, lines) in blocks {
        if !block_carries_secret(&lines, policy) {
            continue;
        }
        out.push_str(&text[cursor..start]);
        out.push_str(GOAL_REDACTED_SECRET);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn wrapped_encoded_blocks(text: &str) -> Vec<(usize, usize, Vec<String>)> {
    let mut blocks = Vec::new();
    let mut block: Option<(usize, usize, Vec<String>)> = None;
    let mut position = 0usize;
    for raw in text.split_inclusive('\n') {
        let line_start = position;
        position += raw.len();
        let without_newline = raw.strip_suffix('\n').unwrap_or(raw);
        let line = without_newline
            .strip_suffix('\r')
            .unwrap_or(without_newline);
        let content = line.trim();
        let encoded_line = !content.is_empty()
            && content.bytes().all(is_encoded_byte)
            && (content.len() >= 8 || block.is_some());
        if encoded_line {
            match block.as_mut() {
                Some((_, end, lines)) => {
                    lines.push(content.to_string());
                    *end = line_start + line.len();
                }
                None => {
                    block = Some((
                        line_start,
                        line_start + line.len(),
                        vec![content.to_string()],
                    ))
                }
            }
        } else if let Some(done) = block.take() {
            blocks.push(done);
        }
    }
    if let Some(done) = block {
        blocks.push(done);
    }
    // A block only matters when at least two lines were joined (single-line
    // encodings are covered by the token scan).
    blocks.retain(|(_, _, lines)| lines.len() >= 2);
    blocks
}

fn block_carries_secret(lines: &[String], policy: &SecretPolicy) -> bool {
    const MAX_TAIL_LINES: usize = 8;
    if lines.len() < 2 {
        return false;
    }
    let mut joined = String::new();
    let mut ends = Vec::with_capacity(lines.len());
    for line in lines {
        joined.push_str(line);
        ends.push(joined.len());
    }
    if joined.len() < 16 {
        return false;
    }
    // The full block, then a bounded number of trailing lines dropped: a
    // wrapped secret followed by a short label/prose line still decodes to
    // the credential as soon as that tail is removed.
    let drops = MAX_TAIL_LINES.min(ends.len() - 2);
    (0..=drops).any(|dropped| {
        let end = ends[ends.len() - 1 - dropped];
        end >= 16 && encoded_token_has_secret(&joined[..end], policy)
    })
}

fn is_encoded_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'-' | b'_')
}

fn encoded_secret_token(text: &str, policy: &SecretPolicy) -> Option<String> {
    for (start, end) in token_runs(text) {
        let token = &text[start..end];
        let (core_start, core_end) = strip_edge_punctuation(token);
        if core_start < core_end && encoded_token_has_secret(&token[core_start..core_end], policy) {
            return Some(token[core_start..core_end].to_string());
        }
    }
    None
}

fn token_runs(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() || matches!(ch, '=' | '&' | '?' | ':' | ';' | ',') {
            if let Some(s) = start.take() {
                runs.push((s, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(s) = start {
        runs.push((s, text.len()));
    }
    runs.into_iter()
}

fn strip_edge_punctuation(token: &str) -> (usize, usize) {
    let mut start = 0;
    let mut end = token.len();
    while let Some(ch) = token[start..].chars().next() {
        if is_edge_punctuation(ch) {
            start += ch.len_utf8();
        } else {
            break;
        }
    }
    while end > start {
        let Some(ch) = token[start..end].chars().next_back() else {
            break;
        };
        if is_edge_punctuation(ch) {
            end -= ch.len_utf8();
        } else {
            break;
        }
    }
    (start, end)
}

fn is_edge_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\''
            | '`'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '<'
            | '>'
            | ','
            | '.'
            | ';'
            | ':'
            | '!'
            | '?'
    )
}

fn encoded_token_has_secret(token: &str, policy: &SecretPolicy) -> bool {
    for candidate in decode_candidates(token) {
        if !scan_secrets(&candidate, policy).is_empty() {
            return true;
        }
        for nested in decode_candidates(&candidate) {
            if !scan_secrets(&nested, policy).is_empty() {
                return true;
            }
        }
    }
    false
}

fn decode_candidates(token: &str) -> Vec<String> {
    let mut out = Vec::new();
    if token.contains('%') {
        if let Some(decoded) = percent_decode(token) {
            out.push(decoded);
        }
    }
    if token.len() >= 16
        && token.len().is_multiple_of(2)
        && token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        if let Some(decoded) = hex_decode(token) {
            out.push(decoded);
        }
    }
    if token.len() >= 16 {
        use base64::Engine as _;
        for engine in [
            &base64::engine::general_purpose::STANDARD,
            &base64::engine::general_purpose::URL_SAFE,
            &base64::engine::general_purpose::STANDARD_NO_PAD,
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        ] {
            if let Ok(bytes) = engine.decode(token) {
                if bytes.len() >= 8 {
                    if let Ok(text) = String::from_utf8(bytes) {
                        out.push(text);
                    }
                }
            }
        }
    }
    out
}

fn percent_decode(token: &str) -> Option<String> {
    let bytes = token.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push((high << 4) | low);
                index += 3;
                changed = true;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    if !changed {
        return None;
    }
    String::from_utf8(out).ok()
}

fn hex_decode(token: &str) -> Option<String> {
    let bytes = token.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let (pairs, _) = bytes.as_chunks::<2>();
    for pair in pairs {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        out.push((high << 4) | low);
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn truncate_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
