//! Bounded console observation (spec §9): console API calls, browser log
//! entries and uncaught exceptions for one page.
//!
//! Console output is attacker-controlled: every stored field is
//! independently byte-capped, the ring is bounded, and an observation gap
//! marks the history incomplete (the page refuses complete-history reads
//! instead of presenting a partial console as the whole one).

use std::collections::VecDeque;

use serde_json::Value;

use crate::capture::redact_url;
use crate::cdp::CdpEvent;

/// Hard cap on one console record's text (UTF-8 bytes).
pub const MAX_CONSOLE_TEXT_BYTES: usize = 8 * 1024;
/// Hard cap on the console source identifier (URL/location).
pub const MAX_CONSOLE_SOURCE_BYTES: usize = 2 * 1024;
/// Hard cap on one argument's rendered form (avoid joining huge objects).
const MAX_ARGUMENT_BYTES: usize = 2 * 1024;
/// Hard cap on retained arguments per call (joining is linear).
const MAX_ARGUMENTS: usize = 32;

/// One bounded console observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleObservation {
    /// Monotonic per-page sequence (stable ordering across event kinds).
    pub sequence: u64,
    /// Console severity: `debug`, `info`, `log`, `warning`, `error` or
    /// `assert` (CDP's own vocabulary); log entries keep their level.
    pub level: String,
    /// Bounded rendered text (arguments joined by a single space).
    pub text: String,
    /// Bounded source URL (query values redacted).
    pub url: Option<String>,
    /// One-based line number when the event carries one.
    pub line: Option<u32>,
    /// One-based column number when the event carries one.
    pub column: Option<u32>,
    /// Wall-clock milliseconds when the page pump observed the event.
    pub timestamp_ms: i64,
    /// At least one field was truncated at its configured cap.
    pub truncated: bool,
}

/// Bounded console tracker. Eviction drops the oldest record and increments
/// `dropped`; a lost observation event latches a gap (sticky).
pub struct ConsoleTracker {
    capacity: usize,
    records: VecDeque<ConsoleObservation>,
    next_sequence: u64,
    dropped: u64,
}

impl ConsoleTracker {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            records: VecDeque::new(),
            next_sequence: 1,
            dropped: 0,
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Bounded snapshot in arrival order.
    pub fn records(&self) -> Vec<ConsoleObservation> {
        self.records.iter().cloned().collect()
    }

    fn push(&mut self, mut observation: ConsoleObservation) {
        observation.sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        while self.records.len() >= self.capacity {
            self.records.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.records.push_back(observation);
    }

    /// Apply one CDP console/log/exception event.
    pub fn apply(&mut self, event: &CdpEvent, now_ms: i64) -> bool {
        match event.method.as_str() {
            "Runtime.consoleAPICalled" => {
                let params = &event.params;
                let level = params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("log")
                    .chars()
                    .take(32)
                    .collect::<String>();
                let mut truncated = false;
                let mut rendered: Vec<String> = Vec::new();
                if let Some(args) = params.get("args").and_then(Value::as_array) {
                    for arg in args.iter().take(MAX_ARGUMENTS) {
                        let text = render_argument(arg);
                        if text.len() > MAX_ARGUMENT_BYTES {
                            truncated = true;
                        }
                        rendered.push(bound(&text, MAX_ARGUMENT_BYTES));
                    }
                    if args.len() > MAX_ARGUMENTS {
                        truncated = true;
                    }
                }
                let joined = rendered.join(" ");
                if joined.len() > MAX_CONSOLE_TEXT_BYTES {
                    truncated = true;
                }
                let stack = params
                    .get("stackTrace")
                    .and_then(|stack| stack.get("callFrames"))
                    .and_then(Value::as_array)
                    .and_then(|frames| frames.first());
                let url = stack
                    .and_then(|frame| frame.get("url"))
                    .and_then(Value::as_str)
                    .map(redact_url)
                    .map(|url| bound(&url, MAX_CONSOLE_SOURCE_BYTES));
                let line = stack
                    .and_then(|frame| frame.get("lineNumber"))
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n.saturating_add(1)).ok());
                let column = stack
                    .and_then(|frame| frame.get("columnNumber"))
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n.saturating_add(1)).ok());
                self.push(ConsoleObservation {
                    sequence: 0,
                    level,
                    text: bound(&joined, MAX_CONSOLE_TEXT_BYTES),
                    url,
                    line,
                    column,
                    timestamp_ms: now_ms,
                    truncated,
                });
                true
            }
            "Log.entryAdded" => {
                let entry = event.params.get("entry").cloned().unwrap_or(Value::Null);
                let level = entry
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info")
                    .chars()
                    .take(32)
                    .collect::<String>();
                let raw = entry
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let url = entry
                    .get("url")
                    .and_then(Value::as_str)
                    .map(redact_url)
                    .map(|url| bound(&url, MAX_CONSOLE_SOURCE_BYTES));
                let line = entry
                    .get("lineNumber")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n.saturating_add(1)).ok());
                self.push(ConsoleObservation {
                    sequence: 0,
                    level,
                    text: bound(raw, MAX_CONSOLE_TEXT_BYTES),
                    url,
                    line,
                    column: None,
                    timestamp_ms: now_ms,
                    truncated: raw.len() > MAX_CONSOLE_TEXT_BYTES,
                });
                true
            }
            "Runtime.exceptionThrown" => {
                let details = event
                    .params
                    .get("exceptionDetails")
                    .cloned()
                    .unwrap_or(Value::Null);
                let mut text = details
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("uncaught exception")
                    .to_string();
                if let Some(description) = details
                    .get("exception")
                    .and_then(|exception| exception.get("description"))
                    .and_then(Value::as_str)
                {
                    text.push_str(": ");
                    text.push_str(description);
                }
                let url = details
                    .get("url")
                    .and_then(Value::as_str)
                    .map(redact_url)
                    .map(|url| bound(&url, MAX_CONSOLE_SOURCE_BYTES));
                let line = details
                    .get("lineNumber")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n.saturating_add(1)).ok());
                let column = details
                    .get("columnNumber")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n.saturating_add(1)).ok());
                self.push(ConsoleObservation {
                    sequence: 0,
                    level: "error".into(),
                    text: bound(&text, MAX_CONSOLE_TEXT_BYTES),
                    url,
                    line,
                    column,
                    timestamp_ms: now_ms,
                    truncated: text.len() > MAX_CONSOLE_TEXT_BYTES,
                });
                true
            }
            _ => false,
        }
    }
}

fn render_argument(arg: &Value) -> String {
    for key in ["value", "unserializableValue", "description"] {
        match arg.get(key) {
            Some(Value::String(text)) => return text.clone(),
            Some(Value::Null) if key == "value" => return "null".into(),
            Some(Value::Bool(value)) => return value.to_string(),
            Some(Value::Number(value)) => return value.to_string(),
            Some(Value::Array(items)) => {
                return items
                    .iter()
                    .map(render_argument)
                    .collect::<Vec<_>>()
                    .join(",");
            }
            Some(Value::Object(object)) => {
                return object
                    .iter()
                    .map(|(key, value)| format!("{key}:{}", render_argument(value)))
                    .collect::<Vec<_>>()
                    .join(",");
            }
            _ => {}
        }
    }
    String::new()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            session_id: Some("s".into()),
            method: method.into(),
            params,
        }
    }

    #[test]
    fn console_ring_is_bounded_and_ordered() {
        let mut tracker = ConsoleTracker::new(3);
        for index in 0..5 {
            assert!(tracker.apply(
                &event(
                    "Runtime.consoleAPICalled",
                    json!({
                        "type": "log",
                        "args": [{"value": format!("entry-{index}")}],
                    }),
                ),
                index,
            ));
        }
        let rows = tracker.records();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].text, "entry-2");
        assert_eq!(rows[2].text, "entry-4");
        assert_eq!(tracker.dropped(), 2);
        assert!(rows
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
    }

    #[test]
    fn hostile_console_fields_are_truncated_not_unbounded() {
        let mut tracker = ConsoleTracker::new(8);
        let huge = "x".repeat(MAX_CONSOLE_TEXT_BYTES * 4);
        assert!(tracker.apply(
            &event(
                "Runtime.consoleAPICalled",
                json!({"type": "error", "args": [{"value": huge}]}),
            ),
            1,
        ));
        let row = &tracker.records()[0];
        assert_eq!(row.text.len(), MAX_ARGUMENT_BYTES);
        assert!(row.truncated);
        assert_eq!(row.level, "error");
    }

    #[test]
    fn exceptions_and_log_entries_are_captured_with_redacted_sources() {
        let mut tracker = ConsoleTracker::new(8);
        assert!(tracker.apply(
            &event(
                "Runtime.exceptionThrown",
                json!({
                    "exceptionDetails": {
                        "text": "Uncaught Error",
                        "url": "http://127.0.0.1:1/app.js?token=secret",
                        "lineNumber": 4,
                        "columnNumber": 2,
                        "exception": {"description": "boom"}
                    }
                }),
            ),
            2,
        ));
        assert!(tracker.apply(
            &event(
                "Log.entryAdded",
                json!({"entry": {"level": "warning", "text": "deprecated", "url": "http://127.0.0.1:1/x.js?key=1"}}),
            ),
            3,
        ));
        let rows = tracker.records();
        assert_eq!(rows[0].level, "error");
        assert!(rows[0].text.contains("boom"));
        assert_eq!(rows[0].line, Some(5));
        assert_eq!(rows[0].column, Some(3));
        assert!(!rows[0].url.as_deref().unwrap().contains("secret"));
        assert_eq!(rows[1].level, "warning");
        assert!(!rows[1].url.as_deref().unwrap().contains("key=1"));
    }

    #[test]
    fn unknown_events_are_ignored() {
        let mut tracker = ConsoleTracker::new(2);
        assert!(!tracker.apply(&event("Network.requestWillBeSent", json!({})), 0));
        assert!(tracker.records().is_empty());
    }
}
