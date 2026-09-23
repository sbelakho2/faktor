//! Network observation (spec §9): bounded request/response records plus
//! on-request body capture. Bodies are fetched only when a caller asks, are
//! capped before decoding, and are truncated with an explicit flag — a
//! hostile response can never balloon memory or the caller's context.

use std::collections::{HashMap, VecDeque};

use serde_json::{json, Value};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::Deadline;

use crate::capture::{decode_cdp_body, CaptureLimits};
use crate::cdp::{CdpClient, CdpEvent};
use crate::error::BrowserError;
use crate::interception::ResourceType;

/// Bounds for network observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkLimits {
    /// Bounded record ring per page.
    pub max_records: usize,
    /// Soft per-body cap (truncation point).
    pub max_body_bytes: usize,
    /// Hard per-body cap (refusal point).
    pub hard_max_body_bytes: usize,
}

/// Independent per-field byte caps. A record can never grow without bound
/// through any single attacker-controlled field.
pub const MAX_REQUEST_ID_BYTES: usize = 256;
pub const MAX_URL_BYTES: usize = 8 * 1024;
pub const MAX_METHOD_BYTES: usize = 32;
pub const MAX_MIME_BYTES: usize = 256;
pub const MAX_FAILURE_TEXT_BYTES: usize = 2 * 1024;
pub const MAX_BLOCKED_REASON_BYTES: usize = 256;

/// UTF-8-safe truncation over one field, with an explicit truncation flag.
fn bound_field(raw: &str, cap: usize) -> (String, bool) {
    if raw.len() <= cap {
        return (raw.to_string(), false);
    }
    let mut end = cap;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    (raw[..end].to_string(), true)
}

impl Default for NetworkLimits {
    fn default() -> Self {
        let capture = CaptureLimits::default();
        Self {
            max_records: capture.max_network_records,
            max_body_bytes: capture.max_body_bytes,
            hard_max_body_bytes: capture.hard_max_body_bytes,
        }
    }
}

impl NetworkLimits {
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.max_records == 0 || self.max_body_bytes == 0 || self.hard_max_body_bytes == 0 {
            return Err(BrowserError::invalid_config("network bounds must be > 0"));
        }
        if self.max_body_bytes > self.hard_max_body_bytes {
            return Err(BrowserError::invalid_config(
                "network max_body_bytes must not exceed hard_max_body_bytes",
            ));
        }
        Ok(())
    }
}

/// One observed request/response pair (bounded fields only).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkRequest {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub resource_type: ResourceType,
    pub status: Option<i64>,
    pub mime_type: Option<String>,
    pub from_cache: bool,
    pub encoded_data_length: Option<u64>,
    pub failed: Option<String>,
    pub blocked_reason: Option<String>,
    /// At least one stored field was truncated at its configured byte cap.
    /// The record is kept for diagnostics; the full value was not retained.
    pub truncated: bool,
}

/// A notice produced while applying an event (failed/blocked requests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkNotice {
    Failed {
        request_id: String,
        url: String,
        error_text: String,
    },
    Blocked {
        request_id: String,
        url: String,
        reason: String,
    },
    LoadingFinished {
        request_id: String,
        encoded_data_length: u64,
    },
}

/// Bounded per-page network tracker. Every stored field is independently
/// capped and every stored URL passes through the structural credential
/// sanitizer; a lost observation event latches an `event_gap` so
/// completeness-gated consumers refuse to answer from partial history.
pub struct NetworkTracker {
    limits: NetworkLimits,
    order: VecDeque<String>,
    records: HashMap<String, NetworkRequest>,
    dropped_records: u64,
    invalidated_events: u64,
    event_gap: u64,
}

impl NetworkTracker {
    pub fn new(limits: NetworkLimits) -> Self {
        Self {
            limits,
            order: VecDeque::new(),
            records: HashMap::new(),
            dropped_records: 0,
            invalidated_events: 0,
            event_gap: 0,
        }
    }

    pub fn limits(&self) -> &NetworkLimits {
        &self.limits
    }

    pub fn dropped_records(&self) -> u64 {
        self.dropped_records
    }

    /// Events dropped because their protocol identity (request id) was over
    /// the bound: an oversized id can never be correlated.
    pub fn invalidated_events(&self) -> u64 {
        self.invalidated_events
    }

    /// Record that the observation stream lost `skipped` events. Sticky: the
    /// history is incomplete from here on.
    pub fn record_event_gap(&mut self, skipped: u64) {
        self.event_gap = self.event_gap.saturating_add(skipped);
    }

    /// Number of observation events known to be missing (0 = complete).
    pub fn event_gap(&self) -> u64 {
        self.event_gap
    }

    /// Reject an event whose request-id identity exceeds the bound.
    fn bounded_request_id(&mut self, params: &Value) -> Option<String> {
        let raw = params.get("requestId").and_then(Value::as_str)?;
        if raw.len() > MAX_REQUEST_ID_BYTES {
            // The id is a protocol identity: truncating it would silently
            // alias unrelated requests, so the whole event is invalid.
            self.invalidated_events = self.invalidated_events.saturating_add(1);
            return None;
        }
        Some(raw.to_string())
    }

    /// Apply one CDP event. Unknown events are ignored (the caller routes
    /// only `Network.*` here, but this stays total).
    pub fn apply(&mut self, event: &CdpEvent) -> Option<NetworkNotice> {
        let params = &event.params;
        let request_id = self.bounded_request_id(params)?;
        match event.method.as_str() {
            "Network.requestWillBeSent" => {
                let raw_url = params
                    .get("request")
                    .and_then(|r| r.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (url, url_truncated) =
                    bound_field(&crate::capture::redact_url(raw_url), MAX_URL_BYTES);
                let (method, method_truncated) = bound_field(
                    params
                        .get("request")
                        .and_then(|r| r.get("method"))
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    MAX_METHOD_BYTES,
                );
                let resource_type = ResourceType::parse(
                    params
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("Other"),
                );
                self.insert(NetworkRequest {
                    request_id,
                    url,
                    method,
                    resource_type,
                    truncated: url_truncated || method_truncated,
                    ..NetworkRequest::default()
                });
                None
            }
            "Network.responseReceived" => {
                if let Some(record) = self.records.get_mut(&request_id) {
                    let response = params.get("response").cloned().unwrap_or(Value::Null);
                    record.status = response.get("status").and_then(Value::as_i64);
                    if let Some(raw_mime) = response.get("mimeType").and_then(Value::as_str) {
                        let (mime, truncated) = bound_field(raw_mime, MAX_MIME_BYTES);
                        record.truncated |= truncated;
                        record.mime_type = Some(mime);
                    }
                    record.from_cache = response
                        .get("fromDiskCache")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        || response
                            .get("fromServiceWorker")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                }
                None
            }
            "Network.loadingFinished" => {
                let length = params
                    .get("encodedDataLength")
                    .and_then(Value::as_f64)
                    .map(|n| n.max(0.0) as u64)
                    .unwrap_or(0);
                if let Some(record) = self.records.get_mut(&request_id) {
                    record.encoded_data_length = Some(length);
                }
                Some(NetworkNotice::LoadingFinished {
                    request_id,
                    encoded_data_length: length,
                })
            }
            "Network.loadingFailed" => {
                let (error_text, error_truncated) = bound_field(
                    params
                        .get("errorText")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown network failure"),
                    MAX_FAILURE_TEXT_BYTES,
                );
                let blocked_reason = params
                    .get("blockedReason")
                    .and_then(Value::as_str)
                    .map(|raw| bound_field(raw, MAX_BLOCKED_REASON_BYTES));
                let url = self
                    .records
                    .get(&request_id)
                    .map(|r| r.url.clone())
                    .unwrap_or_default();
                if let Some(record) = self.records.get_mut(&request_id) {
                    record.truncated |= error_truncated
                        || blocked_reason
                            .as_ref()
                            .map(|(_, truncated)| *truncated)
                            .unwrap_or(false);
                    record.failed = Some(error_text.clone());
                    record.blocked_reason = blocked_reason.as_ref().map(|(value, _)| value.clone());
                }
                match blocked_reason {
                    Some((reason, _)) => Some(NetworkNotice::Blocked {
                        request_id,
                        url,
                        reason,
                    }),
                    None => Some(NetworkNotice::Failed {
                        request_id,
                        url,
                        error_text,
                    }),
                }
            }
            _ => None,
        }
    }

    fn insert(&mut self, record: NetworkRequest) {
        let id = record.request_id.clone();
        if let Some(existing) = self.records.get_mut(&id) {
            *existing = record;
            return;
        }
        while self.order.len() >= self.limits.max_records {
            if let Some(evicted) = self.order.pop_front() {
                self.records.remove(&evicted);
                self.dropped_records = self.dropped_records.saturating_add(1);
            }
        }
        self.order.push_back(id.clone());
        self.records.insert(id, record);
    }

    /// Bounded snapshot in observation order. Lossy on its own: callers that
    /// need a complete history must check [`NetworkTracker::event_gap`] (or
    /// use [`NetworkTracker::json_candidates`] / `Page::network`, which
    /// refuse after a gap).
    pub fn requests(&self) -> Vec<NetworkRequest> {
        self.order
            .iter()
            .filter_map(|id| self.records.get(id).cloned())
            .collect()
    }

    pub fn get(&self, request_id: &str) -> Option<NetworkRequest> {
        self.records.get(request_id).cloned()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// XHR/fetch JSON responses are the primary extraction surface; this
    /// returns them newest-first, bounded by `limit`. After an observation
    /// gap the history is incomplete and the call refuses typed instead of
    /// answering from partial data.
    pub fn json_candidates(&self, limit: usize) -> Result<Vec<NetworkRequest>, BrowserError> {
        if self.event_gap > 0 {
            return Err(BrowserError::EventStreamLagged {
                skipped: self.event_gap,
            });
        }
        let mut out: Vec<NetworkRequest> = self
            .requests()
            .into_iter()
            .filter(|record| {
                matches!(
                    record.resource_type,
                    ResourceType::Xhr | ResourceType::Fetch
                ) && record
                    .status
                    .map(|s| (200..300).contains(&s))
                    .unwrap_or(false)
                    && record
                        .mime_type
                        .as_deref()
                        .map(|m| m.contains("json"))
                        .unwrap_or(false)
            })
            .collect();
        out.reverse();
        out.truncate(limit);
        Ok(out)
    }
}

/// A captured response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedBody {
    pub request_id: String,
    pub url: Option<String>,
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub byte_len: usize,
}

/// Explicit body capture over CDP. Bodies are fetched only when asked for.
pub struct BodyCapturer {
    client: CdpClient,
    session: String,
    limits: NetworkLimits,
}

impl BodyCapturer {
    pub fn new(client: CdpClient, session: impl Into<String>, limits: NetworkLimits) -> Self {
        Self {
            client,
            session: session.into(),
            limits,
        }
    }

    /// Fetch and decode one response body under the configured bounds.
    pub async fn capture(
        &self,
        request_id: &str,
        url: Option<&str>,
        cap_bytes: usize,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<CapturedBody, BrowserError> {
        let effective_cap = cap_bytes.min(self.limits.max_body_bytes).max(1);
        let result = self
            .client
            .send(
                Some(&self.session),
                "Network.getResponseBody",
                json!({ "requestId": request_id }),
                deadline,
                cancel,
            )
            .await?;
        let body = result
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::cdp("Network.getResponseBody returned no body"))?;
        let base64_encoded = result
            .get("base64Encoded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let captured = decode_cdp_body(
            body,
            base64_encoded,
            effective_cap,
            self.limits.hard_max_body_bytes,
        )?;
        Ok(CapturedBody {
            request_id: request_id.to_string(),
            url: url.map(str::to_string),
            bytes: captured.bytes,
            truncated: captured.truncated,
            byte_len: captured.byte_len,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::CdpEvent;

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            session_id: Some("s1".to_string()),
            method: method.to_string(),
            params,
        }
    }

    #[test]
    fn tracker_records_are_bounded_and_ordered() {
        let limits = NetworkLimits {
            max_records: 3,
            ..NetworkLimits::default()
        };
        let mut tracker = NetworkTracker::new(limits);
        for i in 0..5 {
            tracker.apply(&event(
                "Network.requestWillBeSent",
                json!({
                    "requestId": format!("r{i}"),
                    "type": "XHR",
                    "request": {"url": format!("https://first.test/{i}"), "method": "GET"}
                }),
            ));
        }
        let requests = tracker.requests();
        assert_eq!(requests.len(), 3, "the ring must stay bounded");
        assert_eq!(requests[0].request_id, "r2");
        assert_eq!(tracker.dropped_records(), 2);
    }

    #[test]
    fn response_and_failure_updates_are_applied() {
        let mut tracker = NetworkTracker::new(NetworkLimits::default());
        tracker.apply(&event(
            "Network.requestWillBeSent",
            json!({
                "requestId": "r1",
                "type": "XHR",
                "request": {"url": "https://first.test/api", "method": "GET"}
            }),
        ));
        tracker.apply(&event(
            "Network.responseReceived",
            json!({
                "requestId": "r1",
                "type": "XHR",
                "response": {"status": 200, "mimeType": "application/json", "fromDiskCache": true}
            }),
        ));
        let notice = tracker.apply(&event(
            "Network.loadingFinished",
            json!({"requestId": "r1", "encodedDataLength": 123}),
        ));
        assert_eq!(
            notice,
            Some(NetworkNotice::LoadingFinished {
                request_id: "r1".to_string(),
                encoded_data_length: 123
            })
        );
        let record = tracker.get("r1").unwrap();
        assert_eq!(record.status, Some(200));
        assert_eq!(record.mime_type.as_deref(), Some("application/json"));
        assert!(record.from_cache);
        assert_eq!(record.encoded_data_length, Some(123));
        assert!(!record.truncated);
        assert_eq!(tracker.json_candidates(10).unwrap().len(), 1);

        tracker.apply(&event(
            "Network.requestWillBeSent",
            json!({
                "requestId": "r2",
                "type": "Image",
                "request": {"url": "https://tracker.test/p.gif", "method": "GET"}
            }),
        ));
        let notice = tracker.apply(&event(
            "Network.loadingFailed",
            json!({"requestId": "r2", "errorText": "net::ERR_BLOCKED_BY_CLIENT", "blockedReason": "inspector"}),
        ));
        assert!(matches!(notice, Some(NetworkNotice::Blocked { .. })));
        assert_eq!(
            tracker.get("r2").unwrap().failed.as_deref(),
            Some("net::ERR_BLOCKED_BY_CLIENT")
        );
    }

    #[test]
    fn failed_event_without_a_prior_request_still_reports() {
        let mut tracker = NetworkTracker::new(NetworkLimits::default());
        let notice = tracker.apply(&event(
            "Network.loadingFailed",
            json!({"requestId": "ghost", "errorText": "net::ERR_FAILED"}),
        ));
        assert!(matches!(notice, Some(NetworkNotice::Failed { .. })));
    }

    #[test]
    fn oversized_request_id_invalidates_the_whole_event() {
        let mut tracker = NetworkTracker::new(NetworkLimits::default());
        let huge_id = "r".repeat(MAX_REQUEST_ID_BYTES + 1);
        let notice = tracker.apply(&event(
            "Network.requestWillBeSent",
            json!({
                "requestId": huge_id,
                "type": "XHR",
                "request": {"url": "https://first.test/api", "method": "GET"}
            }),
        ));
        assert!(notice.is_none());
        assert_eq!(tracker.len(), 0, "an uncorrelatable id is never stored");
        assert_eq!(tracker.invalidated_events(), 1);
        // The same oversized id on a response cannot alias or update state.
        tracker.apply(&event(
            "Network.responseReceived",
            json!({"requestId": huge_id, "response": {"status": 200}}),
        ));
        assert_eq!(tracker.invalidated_events(), 2);
        // Exactly at the bound is still a valid identity.
        let exact = "r".repeat(MAX_REQUEST_ID_BYTES);
        tracker.apply(&event(
            "Network.requestWillBeSent",
            json!({
                "requestId": exact,
                "type": "XHR",
                "request": {"url": "https://first.test/ok", "method": "GET"}
            }),
        ));
        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.invalidated_events(), 2);
    }

    #[test]
    fn oversized_fields_are_truncated_utf8_safe_with_a_flag() {
        let mut tracker = NetworkTracker::new(NetworkLimits::default());
        let long_url = format!("https://first.test/{}", "é".repeat(MAX_URL_BYTES));
        let long_method = "M".repeat(MAX_METHOD_BYTES + 10);
        tracker.apply(&event(
            "Network.requestWillBeSent",
            json!({
                "requestId": "trunc",
                "type": "XHR",
                "request": {"url": long_url, "method": long_method}
            }),
        ));
        let record = tracker.get("trunc").unwrap();
        assert!(record.truncated, "truncation must be flagged");
        assert!(record.url.len() <= MAX_URL_BYTES);
        assert!(record.url.starts_with("https://first.test/"));
        assert!(std::str::from_utf8(record.url.as_bytes()).is_ok());
        assert_eq!(record.method.len(), MAX_METHOD_BYTES);
        // Response fields have their own independent caps.
        let long_mime = "application/x-".repeat(64);
        tracker.apply(&event(
            "Network.responseReceived",
            json!({
                "requestId": "trunc",
                "response": {"status": 200, "mimeType": long_mime}
            }),
        ));
        assert!(tracker.get("trunc").unwrap().truncated);
        assert!(
            tracker
                .get("trunc")
                .unwrap()
                .mime_type
                .as_deref()
                .unwrap()
                .len()
                <= MAX_MIME_BYTES
        );
        // Failure text and blocked reason are separately bounded.
        tracker.apply(&event(
            "Network.loadingFailed",
            json!({
                "requestId": "trunc",
                "errorText": "E".repeat(MAX_FAILURE_TEXT_BYTES + 100),
                "blockedReason": "B".repeat(MAX_BLOCKED_REASON_BYTES + 100)
            }),
        ));
        let record = tracker.get("trunc").unwrap();
        assert!(record.truncated);
        assert_eq!(
            record.failed.as_deref().unwrap().len(),
            MAX_FAILURE_TEXT_BYTES
        );
        assert_eq!(
            record.blocked_reason.as_deref().unwrap().len(),
            MAX_BLOCKED_REASON_BYTES
        );
    }

    #[test]
    fn credential_query_parameters_are_masked_in_stored_records() {
        let mut tracker = NetworkTracker::new(NetworkLimits::default());
        tracker.apply(&event(
            "Network.requestWillBeSent",
            json!({
                "requestId": "creds",
                "type": "XHR",
                "request": {
                    "url": "https://first.test/api?token=SECRET123&q=shoes&api_key=XYZ",
                    "method": "GET"
                }
            }),
        ));
        let record = tracker.get("creds").unwrap();
        assert!(!record.url.contains("SECRET123"), "{}", record.url);
        assert!(!record.url.contains("XYZ"), "{}", record.url);
        assert!(record.url.contains("q=shoes"));
        assert!(record.url.contains("token=[redacted]"));
    }

    #[test]
    fn event_gap_latches_and_gates_complete_history() {
        let mut tracker = NetworkTracker::new(NetworkLimits::default());
        tracker.apply(&event(
            "Network.requestWillBeSent",
            json!({
                "requestId": "r1",
                "type": "XHR",
                "request": {"url": "https://first.test/api.json", "method": "GET"}
            }),
        ));
        tracker.apply(&event(
            "Network.responseReceived",
            json!({
                "requestId": "r1",
                "response": {"status": 200, "mimeType": "application/json"}
            }),
        ));
        assert_eq!(tracker.event_gap(), 0);
        assert_eq!(tracker.json_candidates(10).unwrap().len(), 1);
        tracker.record_event_gap(7);
        assert_eq!(tracker.event_gap(), 7);
        assert_eq!(
            tracker.json_candidates(10),
            Err(BrowserError::EventStreamLagged { skipped: 7 })
        );
        // Gaps accumulate; the refusal always reports the full loss.
        tracker.record_event_gap(3);
        assert_eq!(
            tracker.json_candidates(10),
            Err(BrowserError::EventStreamLagged { skipped: 10 })
        );
    }
}
