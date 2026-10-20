//! Workspace-scoped LSP diagnostics and code-action projection.
//!
//! Servers publish diagnostics as notifications (`textDocument/publishDiagnostics`).
//! The hub keeps ONE bounded, versioned snapshot per document URI (a later
//! publish replaces the earlier one, and an empty publish clears it — that is
//! the protocol's stale-clearing contract). Everything is capped: the URI
//! count, the per-document diagnostic count, every retained string and the
//! code-action/workspace-edit payloads.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use faktor_core::cancellation::CancellationToken;

use crate::types::{Position, Range, WireError};

/// Hard cap on documents tracked per language server.
pub const MAX_DIAGNOSTIC_DOCUMENTS: usize = 256;
/// Hard cap on diagnostics retained for one document (the server's order is
/// preserved; the tail is dropped with `truncated = true`).
pub const MAX_DIAGNOSTICS_PER_DOCUMENT: usize = 2048;
/// Hard cap on one diagnostic message.
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 8 * 1024;
/// Hard cap on one diagnostic source / code / URI.
pub const MAX_DIAGNOSTIC_FIELD_BYTES: usize = 2 * 1024;
/// Hard cap on retained related-information entries per diagnostic.
pub const MAX_RELATED_INFORMATION: usize = 16;
/// Hard cap on code actions returned for one request.
pub const MAX_CODE_ACTIONS: usize = 128;
/// Hard cap on one code action title.
pub const MAX_CODE_ACTION_TITLE_BYTES: usize = 4 * 1024;
/// Hard cap on the serialized `edit`/`data`/`command` payload of one action.
pub const MAX_CODE_ACTION_PAYLOAD_BYTES: usize = 256 * 1024;

/// LSP `DiagnosticSeverity`, preserved numerically (unknown values are kept
/// as `None` rather than guessed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    fn from_wire(raw: Option<u64>) -> Option<Self> {
        match raw {
            Some(1) => Some(Self::Error),
            Some(2) => Some(Self::Warning),
            Some(3) => Some(Self::Information),
            Some(4) => Some(Self::Hint),
            _ => None,
        }
    }
}

/// One normalized diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Option<DiagnosticSeverity>,
    pub code: Option<String>,
    pub source: Option<String>,
    pub message: String,
    pub tags: Vec<u32>,
}

impl Diagnostic {
    /// The exact LSP diagnostic object for a `textDocument/codeAction`
    /// context (normalized values only; no server-private fields).
    pub fn to_lsp_json(&self) -> Value {
        let severity = match self.severity {
            Some(DiagnosticSeverity::Error) => Some(1),
            Some(DiagnosticSeverity::Warning) => Some(2),
            Some(DiagnosticSeverity::Information) => Some(3),
            Some(DiagnosticSeverity::Hint) => Some(4),
            None => None,
        };
        let mut value = json!({
            "range": range_json(self.range),
            "message": self.message,
        });
        if let Some(severity) = severity {
            value["severity"] = json!(severity);
        }
        if let Some(code) = &self.code {
            value["code"] = json!(code);
        }
        if let Some(source) = &self.source {
            value["source"] = json!(source);
        }
        if !self.tags.is_empty() {
            value["tags"] = json!(self.tags);
        }
        value
    }
}

fn range_json(range: Range) -> Value {
    json!({
        "start": {"line": range.start.line, "character": range.start.character},
        "end": {"line": range.end.line, "character": range.end.character},
    })
}

/// One document's latest published diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosticsSnapshot {
    pub uri: String,
    /// The server's document version when published; `None` when the server
    /// omitted it (the hub still orders snapshots by `sequence`).
    pub version: Option<i64>,
    /// Monotonic hub sequence of this publish.
    pub sequence: u64,
    /// True when the server published more diagnostics than the cap.
    pub truncated: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// Bounded diagnostic hub shared by the client reader thread and async
/// waiters. A publish for a URI with no diagnostics is retained as an EMPTY
/// snapshot: an explicit clear is distinct from "never published".
pub struct DiagnosticsHub {
    inner: Mutex<HubState>,
    /// Active `$/progress` work-done tokens (server-declared work).
    progress: Mutex<std::collections::BTreeSet<String>>,
    notify: tokio::sync::Notify,
}

struct HubState {
    sequence: u64,
    documents: BTreeMap<String, DiagnosticsSnapshot>,
}

impl Default for DiagnosticsHub {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticsHub {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HubState {
                sequence: 0,
                documents: BTreeMap::new(),
            }),
            progress: Mutex::new(std::collections::BTreeSet::new()),
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Apply one `textDocument/publishDiagnostics` notification. Returns
    /// `false` for malformed notifications (they are dropped, never guessed).
    pub fn apply_publish(&self, params: &Value) -> bool {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return false;
        };
        if uri.is_empty() || uri.len() > MAX_DIAGNOSTIC_FIELD_BYTES {
            return false;
        }
        let version = params.get("version").and_then(Value::as_i64);
        let raw = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut truncated = raw.len() > MAX_DIAGNOSTICS_PER_DOCUMENT;
        let mut diagnostics = Vec::with_capacity(raw.len().min(MAX_DIAGNOSTICS_PER_DOCUMENT));
        for item in raw.iter().take(MAX_DIAGNOSTICS_PER_DOCUMENT) {
            match parse_diagnostic(item) {
                Ok(diagnostic) => diagnostics.push(diagnostic),
                Err(_) => truncated = true,
            }
        }
        let mut state = faktor_core_lock(&self.inner);
        state.sequence = state.sequence.saturating_add(1);
        let snapshot = DiagnosticsSnapshot {
            uri: uri.to_string(),
            version,
            sequence: state.sequence,
            truncated,
            diagnostics,
        };
        // Bound the URI map: evict the least-recently published document.
        while state.documents.len() >= MAX_DIAGNOSTIC_DOCUMENTS
            && !state.documents.contains_key(uri)
        {
            let Some(oldest) = state
                .documents
                .iter()
                .min_by_key(|(_, snapshot)| snapshot.sequence)
                .map(|(uri, _)| uri.clone())
            else {
                break;
            };
            state.documents.remove(&oldest);
        }
        state.documents.insert(uri.to_string(), snapshot);
        drop(state);
        self.notify.notify_waiters();
        true
    }

    /// Drop one document's diagnostics (didClose). A later publish recreates
    /// the snapshot; a stale publish after close can still arrive from a
    /// server, so callers that reopen read the new snapshot by sequence.
    pub fn clear(&self, uri: &str) {
        faktor_core_lock(&self.inner).documents.remove(uri);
        self.notify.notify_waiters();
    }

    /// The latest snapshot for one URI.
    pub fn snapshot(&self, uri: &str) -> Option<DiagnosticsSnapshot> {
        faktor_core_lock(&self.inner).documents.get(uri).cloned()
    }

    /// Every retained snapshot (bounded by [`MAX_DIAGNOSTIC_DOCUMENTS`]).
    pub fn snapshot_all(&self) -> Vec<DiagnosticsSnapshot> {
        faktor_core_lock(&self.inner)
            .documents
            .values()
            .cloned()
            .collect()
    }

    /// Wait for the first post-baseline publish, then keep waiting until the
    /// hub is quiet for `settle` (or the overall timeout). Servers commonly
    /// publish an early EMPTY snapshot while analysis is still running; a
    /// settled snapshot is the honest answer for both a clean document and a
    /// document whose diagnostics arrived later.
    pub async fn wait_settled(
        &self,
        uri: &str,
        after: u64,
        min_version: Option<i64>,
        settle: Duration,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Option<DiagnosticsSnapshot> {
        let overall = tokio::time::Instant::now() + timeout;
        let mut last = self
            .wait_after(uri, after, min_version, timeout, cancel)
            .await?;
        loop {
            if tokio::time::Instant::now() >= overall {
                return self.snapshot(uri).or(Some(last));
            }
            if let Some(token) = cancel {
                if token.is_cancelled() {
                    return self.snapshot(uri).or(Some(last));
                }
            }
            if self.progress_active() > 0 {
                // Server-declared work is still running: an early EMPTY
                // publish is not final. Wait for the next signal.
                let notified = self.notify.notified();
                let slice = Duration::from_millis(100).min(settle);
                if let Some(token) = cancel {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => return self.snapshot(uri).or(Some(last)),
                        _ = tokio::time::sleep(slice) => {}
                        _ = notified => {}
                    }
                } else {
                    tokio::select! {
                        biased;
                        _ = tokio::time::sleep(slice) => {}
                        _ = notified => {}
                    }
                }
                if let Some(snapshot) = self.snapshot(uri) {
                    last = snapshot;
                }
                continue;
            }
            // Quiet window: no server work, and no publish for `settle`.
            let before = last.sequence;
            let quiet = Duration::from_millis(settle.as_millis() as u64)
                .min(overall.saturating_duration_since(tokio::time::Instant::now()));
            if quiet.is_zero() {
                return self.snapshot(uri).or(Some(last));
            }
            let notified = self.notify.notified();
            if let Some(token) = cancel {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => return self.snapshot(uri).or(Some(last)),
                    _ = tokio::time::sleep(quiet) => {}
                    _ = notified => {}
                }
            } else {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep(quiet) => {}
                    _ = notified => {}
                }
            }
            if let Some(snapshot) = self.snapshot(uri) {
                last = snapshot;
            }
            if last.sequence == before && self.progress_active() == 0 {
                return Some(last);
            }
        }
    }

    /// Apply one `$/progress` notification. `begin` registers a token,
    /// `end` removes it; a `report` only proves the server is alive.
    pub fn apply_progress(&self, params: &Value) -> bool {
        let Some(token) = params.get("token") else {
            return false;
        };
        let token = match token {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            _ => return false,
        };
        if token.is_empty() || token.len() > MAX_DIAGNOSTIC_FIELD_BYTES {
            return false;
        }
        let kind = params
            .get("value")
            .and_then(|value| value.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let changed = match kind {
            "begin" => {
                let mut progress = faktor_core_lock(&self.progress);
                progress.insert(token)
            }
            "end" => {
                let mut progress = faktor_core_lock(&self.progress);
                progress.remove(&token)
            }
            _ => false,
        };
        if changed {
            self.notify.notify_waiters();
        }
        true
    }

    /// Number of server-declared active work-done tokens.
    pub fn progress_active(&self) -> usize {
        faktor_core_lock(&self.progress).len()
    }

    /// Current global sequence (a baseline for [`Self::wait_after`]).
    pub fn sequence(&self) -> u64 {
        faktor_core_lock(&self.inner).sequence
    }

    /// Wait until `uri` has a snapshot with `sequence > after` (and, when
    /// `min_version` is supplied, a version at or above it). Deadline and
    /// cancellation are both honored; a timeout returns the last snapshot
    /// that existed at the deadline (or `None`).
    pub async fn wait_after(
        &self,
        uri: &str,
        after: u64,
        min_version: Option<i64>,
        timeout: Duration,
        cancel: Option<&CancellationToken>,
    ) -> Option<DiagnosticsSnapshot> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(snapshot) = self.snapshot(uri) {
                let version_ok = match (min_version, snapshot.version) {
                    (Some(minimum), Some(version)) => version >= minimum,
                    // A server that omits versions is trusted to have
                    // processed the didOpen that preceded this wait (the
                    // sequence baseline orders the publish).
                    (Some(_), None) => true,
                    (None, _) => true,
                };
                if snapshot.sequence > after && version_ok {
                    return Some(snapshot);
                }
            }
            let notified = self.notify.notified();
            if let Some(token) = cancel {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => return self.snapshot(uri),
                    _ = tokio::time::sleep_until(deadline) => return self.snapshot(uri),
                    _ = notified => {}
                }
            } else {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(deadline) => return self.snapshot(uri),
                    _ = notified => {}
                }
            }
        }
    }
}

fn faktor_core_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}

fn bound(raw: &str, cap: usize) -> String {
    if raw.len() <= cap {
        return raw.to_string();
    }
    let mut end = cap;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_string()
}

fn parse_diagnostic(value: &Value) -> Result<Diagnostic, WireError> {
    let object = value
        .as_object()
        .ok_or_else(|| WireError("diagnostic is not an object".into()))?;
    let range = Range::from_value(
        object
            .get("range")
            .ok_or_else(|| WireError("diagnostic range is missing".into()))?,
    )?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| WireError("diagnostic message is missing".into()))?;
    let code = object.get("code").and_then(|code| match code {
        Value::String(text) => Some(bound(text, MAX_DIAGNOSTIC_FIELD_BYTES)),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    });
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .map(|text| bound(text, MAX_DIAGNOSTIC_FIELD_BYTES));
    let tags = object
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .take(8)
                .filter_map(Value::as_u64)
                .filter_map(|tag| u32::try_from(tag).ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(Diagnostic {
        range,
        severity: DiagnosticSeverity::from_wire(object.get("severity").and_then(Value::as_u64)),
        code,
        source,
        message: bound(message, MAX_DIAGNOSTIC_MESSAGE_BYTES),
        tags,
    })
}

/// One normalized code action. `edit` is the server's workspace edit payload
/// (validated and byte-capped); applying it is the HOST's explicit action —
/// this surface never mutates a document.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CodeAction {
    pub title: String,
    pub kind: Option<String>,
    pub is_preferred: bool,
    pub disabled_reason: Option<String>,
    pub edit: Option<Value>,
    pub command: Option<Value>,
    pub data: Option<Value>,
}

/// Parse and bound one `textDocument/codeAction` result.
pub fn parse_code_actions(result: &Value) -> Result<Vec<CodeAction>, WireError> {
    if result.is_null() {
        // LSP allows `null` when the server has no action for the range.
        return Ok(Vec::new());
    }
    let items = result
        .as_array()
        .ok_or_else(|| WireError("code action result is not an array".into()))?;
    let mut actions = Vec::with_capacity(items.len().min(MAX_CODE_ACTIONS));
    for item in items.iter().take(MAX_CODE_ACTIONS) {
        if item.get("title").and_then(Value::as_str).is_none() {
            continue;
        }
        let title = bound(
            item.get("title")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            MAX_CODE_ACTION_TITLE_BYTES,
        );
        let kind = item
            .get("kind")
            .and_then(Value::as_str)
            .map(|kind| bound(kind, MAX_DIAGNOSTIC_FIELD_BYTES));
        let is_preferred = item
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let disabled_reason = item
            .get("disabled")
            .and_then(|disabled| disabled.get("reason"))
            .and_then(Value::as_str)
            .map(|reason| bound(reason, MAX_DIAGNOSTIC_MESSAGE_BYTES));
        let edit = match item.get("edit") {
            Some(edit) => {
                validate_workspace_edit(edit)?;
                Some(edit.clone())
            }
            None => None,
        };
        let command = item.get("command").map(cap_payload).transpose()?;
        let data = item.get("data").map(cap_payload).transpose()?;
        actions.push(CodeAction {
            title,
            kind,
            is_preferred,
            disabled_reason,
            edit,
            command,
            data,
        });
    }
    Ok(actions)
}

fn cap_payload(value: &Value) -> Result<Value, WireError> {
    let rendered = value.to_string();
    if rendered.len() > MAX_CODE_ACTION_PAYLOAD_BYTES {
        return Err(WireError("code action payload exceeds the bound".into()));
    }
    Ok(value.clone())
}

/// Validate that an edit contains only recognized, well-formed range edits.
/// Unknown top-level shapes are refused (the host must never apply a
/// half-understood edit).
fn validate_workspace_edit(edit: &Value) -> Result<(), WireError> {
    let object = edit
        .as_object()
        .ok_or_else(|| WireError("workspace edit is not an object".into()))?;
    if edit.to_string().len() > MAX_CODE_ACTION_PAYLOAD_BYTES {
        return Err(WireError("workspace edit exceeds the bound".into()));
    }
    if let Some(changes) = object.get("changes") {
        let changes = changes
            .as_object()
            .ok_or_else(|| WireError("workspace edit changes is not an object".into()))?;
        for edits in changes.values() {
            validate_text_edits(edits)?;
        }
    }
    if let Some(document_changes) = object.get("documentChanges") {
        let list = document_changes
            .as_array()
            .ok_or_else(|| WireError("documentChanges is not an array".into()))?;
        for change in list {
            if change.get("textDocument").is_some() {
                validate_text_edits(
                    change
                        .get("edits")
                        .ok_or_else(|| WireError("textDocumentEdit edits missing".into()))?,
                )?;
            } else if change.get("kind").is_some() {
                // Resource operation (create/rename/delete): recognized but
                // never applied by this surface.
            } else {
                return Err(WireError("unknown documentChanges entry".into()));
            }
        }
    }
    if object.get("changes").is_none() && object.get("documentChanges").is_none() {
        return Err(WireError(
            "workspace edit carries neither changes nor documentChanges".into(),
        ));
    }
    Ok(())
}

fn validate_text_edits(value: &Value) -> Result<(), WireError> {
    let edits = value
        .as_array()
        .ok_or_else(|| WireError("text edits is not an array".into()))?;
    for edit in edits {
        Range::from_value(
            edit.get("range")
                .ok_or_else(|| WireError("text edit range missing".into()))?,
        )?;
        if edit.get("newText").and_then(Value::as_str).is_none() {
            return Err(WireError("text edit newText missing".into()));
        }
    }
    Ok(())
}

/// A code-action request's explicit range.
pub fn request_range(start: Position, end: Position) -> Range {
    Range { start, end }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish(uri: &str, version: Option<i64>, diagnostics: Value) -> Value {
        json!({"uri": uri, "version": version, "diagnostics": diagnostics})
    }

    #[test]
    fn publish_replaces_and_empty_publish_clears() {
        let hub = DiagnosticsHub::new();
        assert!(hub.apply_publish(&publish(
            "file:///w/a.rs",
            Some(1),
            json!([{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}},
                "severity": 1,
                "source": "rust-analyzer",
                "message": "mismatched types"
            }])
        )));
        let first = hub.snapshot("file:///w/a.rs").expect("snapshot");
        assert_eq!(first.version, Some(1));
        assert_eq!(first.diagnostics.len(), 1);
        assert_eq!(
            first.diagnostics[0].severity,
            Some(DiagnosticSeverity::Error)
        );
        assert_eq!(
            first.diagnostics[0].source.as_deref(),
            Some("rust-analyzer")
        );

        assert!(hub.apply_publish(&publish("file:///w/a.rs", Some(2), json!([]))));
        let cleared = hub.snapshot("file:///w/a.rs").expect("cleared snapshot");
        assert_eq!(cleared.version, Some(2));
        assert!(cleared.diagnostics.is_empty());
        assert!(cleared.sequence > first.sequence);
    }

    #[test]
    fn hostile_publishes_are_bounded_and_malformed_entries_dropped() {
        let hub = DiagnosticsHub::new();
        let mut items = Vec::new();
        for index in 0..(MAX_DIAGNOSTICS_PER_DOCUMENT + 10) {
            items.push(json!({
                "range": {"start": {"line": index, "character": 0}, "end": {"line": index, "character": 1}},
                "severity": 2,
                "message": "x".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES + 50)
            }));
        }
        items.push(json!({"range": {"start": {"line": 1, "character": 2}, "end": {"line": 0, "character": 0}}, "message": "reversed"}));
        assert!(hub.apply_publish(&publish("file:///w/a.rs", None, json!(items))));
        let snapshot = hub.snapshot("file:///w/a.rs").unwrap();
        assert_eq!(snapshot.diagnostics.len(), MAX_DIAGNOSTICS_PER_DOCUMENT);
        assert!(snapshot.truncated);
        assert_eq!(
            snapshot.diagnostics[0].message.len(),
            MAX_DIAGNOSTIC_MESSAGE_BYTES
        );
    }

    #[test]
    fn document_map_is_bounded() {
        let hub = DiagnosticsHub::new();
        for index in 0..(MAX_DIAGNOSTIC_DOCUMENTS + 5) {
            assert!(hub.apply_publish(&publish(
                &format!("file:///w/{index}.rs"),
                Some(1),
                json!([])
            )));
        }
        assert_eq!(hub.snapshot_all().len(), MAX_DIAGNOSTIC_DOCUMENTS);
        assert!(hub.snapshot("file:///w/0.rs").is_none(), "oldest evicted");
    }

    #[tokio::test]
    async fn wait_after_observes_a_later_publish() {
        let hub = std::sync::Arc::new(DiagnosticsHub::new());
        let baseline = hub.sequence();
        let writer = hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            writer.apply_publish(&publish(
                "file:///w/a.rs",
                Some(7),
                json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "message": "later"}]),
            ));
        });
        let snapshot = hub
            .wait_after(
                "file:///w/a.rs",
                baseline,
                Some(7),
                Duration::from_secs(2),
                None,
            )
            .await
            .expect("publish observed");
        assert_eq!(snapshot.version, Some(7));
        assert_eq!(snapshot.diagnostics[0].message, "later");
    }

    #[test]
    fn code_actions_are_normalized_and_payload_bounded() {
        let result = json!([
            {
                "title": "Fix type",
                "kind": "quickfix",
                "isPreferred": true,
                "edit": {"changes": {"file:///w/a.rs": [{
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                    "newText": "x"
                }]}}
            },
            {"title": "Explain", "command": {"command": "faktor.explain", "arguments": []}},
            {"title": "No edit", "data": {"k": 1}}
        ]);
        let actions = parse_code_actions(&result).expect("parse");
        assert_eq!(actions.len(), 3);
        assert!(actions[0].is_preferred);
        assert!(actions[0].edit.is_some());
        assert_eq!(
            actions[1].command.as_ref().unwrap()["command"],
            "faktor.explain"
        );

        let hostile = json!([{
            "title": "bad",
            "edit": {"changes": {"file:///w/a.rs": [{
                "range": {"start": {"line": 9, "character": 9}, "end": {"line": 0, "character": 0}},
                "newText": "x"
            }]}}
        }]);
        assert!(parse_code_actions(&hostile).is_err());

        let unknown_edit = json!([{"title": "bad", "edit": {"surprise": true}}]);
        assert!(parse_code_actions(&unknown_edit).is_err());
    }
}
