//! CDP transport: one WebSocket session to Chromium's DevTools endpoint.
//!
//! The client correlates command responses by `id`, publishes protocol
//! events on a bounded broadcast channel, enforces a per-command deadline, a
//! message-size bound, and observes caller cancellation. A closed socket
//! fails every in-flight command typed (never a hang); the supervisor's
//! registry remains the authority on whether the child actually died.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex, Notify};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::Deadline;

use crate::error::BrowserError;
use crate::timeutil::deadline_instant;

/// Absolute ceilings for CDP capacities. A configured bound above these (or
/// zero) is a configuration error, never a silently unbounded stream.
pub const CDP_MAX_MESSAGE_CEILING_BYTES: usize = 64 * 1024 * 1024;
pub const CDP_MAX_CAPACITY_CEILING: usize = 65_536;

/// CDP client bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpConfig {
    /// Maximum accepted inbound message size (CDP events/bodies). Applied at
    /// the WebSocket layer (`max_message_size`/`max_frame_size`) AND as an
    /// application-level check (defense in depth).
    pub max_message_bytes: usize,
    /// Bounded observation fan-out capacity; a lagging consumer is told it
    /// lagged.
    pub event_capacity: usize,
    /// Bounded per-session queue for critical events. On overflow the
    /// session is failed: the page must never continue with unknowable
    /// state.
    pub critical_event_capacity: usize,
}

impl Default for CdpConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: 8 * 1024 * 1024,
            event_capacity: 2048,
            critical_event_capacity: 256,
        }
    }
}

impl CdpConfig {
    /// Zero, inverted or absurd capacities are typed configuration errors.
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.max_message_bytes == 0 || self.max_message_bytes > CDP_MAX_MESSAGE_CEILING_BYTES {
            return Err(BrowserError::invalid_config(format!(
                "cdp max_message_bytes must be 1..={CDP_MAX_MESSAGE_CEILING_BYTES}"
            )));
        }
        if self.event_capacity == 0 || self.event_capacity > CDP_MAX_CAPACITY_CEILING {
            return Err(BrowserError::invalid_config(format!(
                "cdp event_capacity must be 1..={CDP_MAX_CAPACITY_CEILING}"
            )));
        }
        if self.critical_event_capacity == 0
            || self.critical_event_capacity > CDP_MAX_CAPACITY_CEILING
        {
            return Err(BrowserError::invalid_config(format!(
                "cdp critical_event_capacity must be 1..={CDP_MAX_CAPACITY_CEILING}"
            )));
        }
        Ok(())
    }
}

/// How a CDP event must be handled for loss safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClass {
    /// Loss leaves the page in an unknowable state (a paused request that is
    /// never resumed, a missed termination or lifecycle transition). These
    /// travel on a dedicated bounded per-session queue.
    Critical,
    /// Observation-only events (network records, downloads, JS exceptions).
    /// They may stay lossy, but a gap is recorded and anything needing a
    /// complete history refuses to answer afterwards.
    Observation,
}

/// Classify one CDP event method.
pub fn classify_event(method: &str) -> EventClass {
    match method {
        "Fetch.requestPaused"
        | "Target.targetCrashed"
        | "Target.detachedFromTarget"
        | "Inspector.detached"
        | "Page.frameNavigated"
        | "Page.domContentEventFired"
        | "Page.loadEventFired" => EventClass::Critical,
        _ => EventClass::Observation,
    }
}

/// Why a per-session critical event stream stopped being usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EventStreamError {
    /// The bounded queue overflowed: the session's state is unknowable.
    Lagged { skipped: u64 },
    /// The client closed.
    Closed,
}

struct CriticalState {
    queue: VecDeque<CdpEvent>,
    skipped: u64,
    failed: bool,
    closed: bool,
}

/// A bounded, per-session queue for critical events. Overflow latches the
/// queue failed (all pending events are abandoned) and wakes the consumer
/// with an explicit lag; the stream never silently continues.
/// Immediate overflow consumer (the owning page's `fail_stream`), invoked by
/// [`CriticalQueue::push`] from the reader path.
pub(crate) type OverflowHook = Arc<dyn Fn(u64) + Send + Sync>;

pub(crate) struct CriticalQueue {
    capacity: usize,
    state: Mutex<CriticalState>,
    notify: Notify,
    /// Immediate overflow consumer (the owning page's `fail_stream`). Called
    /// from the reader task the moment an overflow latches, so the page fails
    /// with the typed lag WITHOUT waiting for the event pump to come back to
    /// `recv()` (a pump can be parked in a per-event handler deadline for
    /// seconds; lag latency must not be a multiple of that).
    overflow_hook: Mutex<Option<OverflowHook>>,
}

impl CriticalQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(CriticalState {
                queue: VecDeque::new(),
                skipped: 0,
                failed: false,
                closed: false,
            }),
            notify: Notify::new(),
            overflow_hook: Mutex::new(None),
        }
    }

    /// Register the immediate overflow consumer. One page owns one session's
    /// queue; the last registration wins by design.
    pub(crate) fn set_overflow_hook(&self, hook: OverflowHook) {
        *self.overflow_hook.lock().unwrap() = Some(hook);
    }

    fn push(&self, event: CdpEvent) {
        let mut overflowed = None;
        {
            let mut state = self.state.lock().unwrap();
            if state.failed {
                return;
            }
            if state.queue.len() >= self.capacity {
                // Abandon every queued event plus this one: the session state
                // is unknowable from here on.
                state.skipped = state.queue.len() as u64 + 1;
                state.queue.clear();
                state.failed = true;
                overflowed = Some(state.skipped);
            } else {
                state.queue.push_back(event);
            }
        }
        self.notify.notify_one();
        if let Some(skipped) = overflowed {
            let hook = self
                .overflow_hook
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Some(hook) = hook {
                hook(skipped);
            }
        }
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.notify.notify_one();
    }

    pub(crate) async fn recv(&self) -> Result<CdpEvent, EventStreamError> {
        loop {
            // A permit is stored by `notify_one`, so a push between the check
            // and the await can never be lost.
            let notified = self.notify.notified();
            {
                let mut state = self.state.lock().unwrap();
                if state.failed {
                    return Err(EventStreamError::Lagged {
                        skipped: state.skipped.max(1),
                    });
                }
                if let Some(event) = state.queue.pop_front() {
                    return Ok(event);
                }
                if state.closed {
                    return Err(EventStreamError::Closed);
                }
            }
            notified.await;
        }
    }
}

/// The two event streams of one attached session.
pub(crate) struct SessionEvents {
    pub(crate) critical: Arc<CriticalQueue>,
    pub(crate) observations: broadcast::Receiver<CdpEvent>,
}

/// One CDP event (`method` + `params`), routed to its session when the
/// browser attached targets in flattened mode.
#[derive(Debug, Clone, PartialEq)]
pub struct CdpEvent {
    pub session_id: Option<String>,
    pub method: String,
    pub params: Value,
}

/// The outcome of one CDP command.
#[derive(Debug, Clone, PartialEq)]
pub enum CdpOutcome {
    Ok(Value),
    Error { code: i64, message: String },
}

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type WsSink = futures_util::stream::SplitSink<WsStream, Message>;

struct CdpInner {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<CdpOutcome>>>,
    events: broadcast::Sender<CdpEvent>,
    session_critical: Mutex<HashMap<String, Arc<CriticalQueue>>>,
    closed: AtomicBool,
    closed_reason: Mutex<Option<String>>,
    writer: AsyncMutex<Option<WsSink>>,
    reader: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
    config: CdpConfig,
}

impl CdpInner {
    fn mark_closed(&self, reason: impl Into<String>) {
        let reason = reason.into();
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut slot = self.closed_reason.lock().unwrap();
            if slot.is_none() {
                *slot = Some(reason);
            }
        }
        for queue in self.session_critical.lock().unwrap().values() {
            queue.close();
        }
        let pending: Vec<(u64, oneshot::Sender<CdpOutcome>)> = {
            let mut map = self.pending.lock().unwrap();
            map.drain().collect()
        };
        for (_, tx) in pending {
            let _ = tx.send(CdpOutcome::Error {
                code: -1,
                message: "cdp connection closed".to_string(),
            });
        }
    }

    fn critical_queue(&self, session: &str) -> Arc<CriticalQueue> {
        self.session_critical
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_insert_with(|| Arc::new(CriticalQueue::new(self.config.critical_event_capacity)))
            .clone()
    }
}

/// A CDP client handle. Cheap to clone; all clones share one socket.
#[derive(Clone)]
pub struct CdpClient {
    inner: Arc<CdpInner>,
}

impl std::fmt::Debug for CdpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdpClient")
            .field("closed", &self.is_closed())
            .field("pending", &self.inner.pending.lock().unwrap().len())
            .finish()
    }
}

impl CdpClient {
    /// Connect to a `ws://` DevTools endpoint. Cancellation and deadline are
    /// honored during the handshake. The URL is revalidated here (defense in
    /// depth: only a loopback DevTools browser endpoint is ever dialed), and
    /// the configured message bound is applied at the WebSocket layer as
    /// well as in the application checks.
    pub async fn connect(
        ws_url: &str,
        cancel: &CancellationToken,
        deadline: Deadline,
        config: CdpConfig,
    ) -> Result<Self, BrowserError> {
        config.validate()?;
        crate::launch::validate_devtools_ws_url(ws_url)
            .map_err(|detail| BrowserError::cdp(format!("refusing cdp endpoint: {detail}")))?;
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        let inner = Arc::new(CdpInner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            events,
            session_critical: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            closed_reason: Mutex::new(None),
            writer: AsyncMutex::new(None),
            reader: AsyncMutex::new(None),
            config: config.clone(),
        });
        let ws_config = WebSocketConfig::default()
            .max_message_size(Some(config.max_message_bytes))
            .max_frame_size(Some(config.max_message_bytes));
        let connect = connect_async_with_config(ws_url, Some(ws_config), false);
        tokio::pin!(connect);
        let (ws, _response) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(BrowserError::Cancelled),
            result = tokio::time::timeout_at(deadline_instant(deadline), &mut connect) => {
                match result {
                    Ok(Ok(pair)) => pair,
                    Ok(Err(e)) => {
                        return Err(BrowserError::cdp(format!(
                            "devtools websocket connect failed: {e}"
                        )))
                    }
                    Err(_) => {
                        return Err(BrowserError::Deadline {
                            detail: "devtools websocket connect".to_string(),
                        })
                    }
                }
            }
        };
        let (sink, stream) = ws.split();
        *inner.writer.lock().await = Some(sink);
        let reader_inner = inner.clone();
        let handle = tokio::spawn(reader_loop(stream, reader_inner));
        *inner.reader.lock().await = Some(handle);
        Ok(Self { inner })
    }

    /// Send one CDP command and await its response. `session` is the
    /// flattened target session id when the command is target-scoped.
    pub async fn send(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Value, BrowserError> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(self.closed_error());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let text = message.to_string();
        if text.len() > self.inner.config.max_message_bytes {
            return Err(BrowserError::ResponseTooLarge {
                limit_bytes: self.inner.config.max_message_bytes,
                observed_bytes: Some(text.len()),
            });
        }
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        let write_result = {
            let mut writer = self.inner.writer.lock().await;
            match writer.as_mut() {
                Some(sink) => sink.send(Message::text(text)).await.map_err(|e| {
                    BrowserError::cdp(format!("devtools websocket write failed: {e}"))
                }),
                None => Err(self.closed_error()),
            }
        };
        if let Err(error) = write_result {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        tokio::pin!(rx);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.inner.pending.lock().unwrap().remove(&id);
                Err(BrowserError::Cancelled)
            }
            result = tokio::time::timeout_at(deadline_instant(deadline), &mut rx) => {
                match result {
                    Ok(Ok(CdpOutcome::Ok(value))) => Ok(value),
                    Ok(Ok(CdpOutcome::Error { code, message })) => {
                        if self.inner.closed.load(Ordering::SeqCst) {
                            // The connection died while this command was in
                            // flight: a transport failure, not a protocol
                            // refusal by a live browser.
                            Err(self.closed_error())
                        } else {
                            Err(BrowserError::CdpCommand {
                                method: method.to_string(),
                                code,
                                message,
                            })
                        }
                    }
                    Ok(Err(_recv)) => {
                        self.inner.pending.lock().unwrap().remove(&id);
                        Err(self.closed_error())
                    }
                    Err(_) => {
                        self.inner.pending.lock().unwrap().remove(&id);
                        Err(BrowserError::Deadline { detail: format!("cdp command {method}") })
                    }
                }
            }
        }
    }

    /// Subscribe to the observation (non-critical) event fan-out. A lagging
    /// subscriber observes `Lagged` and must record the gap (bounded
    /// fan-out, never unbounded buffering).
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.inner.events.subscribe()
    }

    /// Subscribe to one attached session's event streams: a dedicated
    /// bounded critical queue plus the shared observation fan-out. Critical
    /// events (paused requests, target/session termination, lifecycle
    /// transitions) are delivered exactly once through the queue and never
    /// through the lossy broadcast.
    pub(crate) fn subscribe_session(&self, session_id: &str) -> SessionEvents {
        SessionEvents {
            critical: self.inner.critical_queue(session_id),
            observations: self.inner.events.subscribe(),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    pub fn closed_reason(&self) -> Option<String> {
        self.inner.closed_reason.lock().unwrap().clone()
    }

    fn closed_error(&self) -> BrowserError {
        BrowserError::cdp(match self.closed_reason() {
            Some(reason) => format!("cdp connection closed: {reason}"),
            None => "cdp connection closed".to_string(),
        })
    }

    /// Close the socket deterministically: pending commands fail typed, the
    /// reader task is joined, and the writer is dropped.
    pub async fn close(&self) {
        let sink = self.inner.writer.lock().await.take();
        if let Some(mut sink) = sink {
            let _ = sink.close().await;
        }
        self.inner.mark_closed("closed by caller");
        if let Some(handle) = self.inner.reader.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

async fn reader_loop(
    mut stream: futures_util::stream::SplitStream<WsStream>,
    inner: Arc<CdpInner>,
) {
    let reason = loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                if text.len() > inner.config.max_message_bytes {
                    break format!(
                        "inbound cdp message of {} bytes exceeds the {} byte bound",
                        text.len(),
                        inner.config.max_message_bytes
                    );
                }
                match serde_json::from_str::<Value>(&text) {
                    Ok(value) => route_message(value, &inner),
                    Err(e) => {
                        tracing::warn!(error = %e, "cdp: malformed json message dropped");
                    }
                }
            }
            Some(Ok(Message::Binary(bytes))) => {
                if bytes.len() > inner.config.max_message_bytes {
                    break format!(
                        "inbound cdp binary of {} bytes exceeds the bound",
                        bytes.len()
                    );
                }
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => route_message(value, &inner),
                    Err(e) => {
                        tracing::warn!(error = %e, "cdp: malformed binary message dropped");
                    }
                }
            }
            Some(Ok(Message::Close(_))) => break "devtools closed the connection".to_string(),
            Some(Ok(_)) => {}
            Some(Err(e)) => break format!("devtools websocket error: {e}"),
            None => break "devtools websocket ended".to_string(),
        }
    };
    inner.mark_closed(reason);
}

fn route_message(value: Value, inner: &Arc<CdpInner>) {
    if let Some(id) = value.get("id").and_then(Value::as_u64) {
        let outcome = match value.get("error") {
            Some(error) => CdpOutcome::Error {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown cdp error")
                    .to_string(),
            },
            None => CdpOutcome::Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(outcome);
        }
        return;
    }
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let event = CdpEvent {
        session_id: session_id.clone(),
        method: method.to_string(),
        params: params.clone(),
    };
    match classify_event(method) {
        EventClass::Critical => {
            // Session-scoped critical events go to their session's bounded
            // queue only: never the lossy broadcast. `Target.detachedFromTarget`
            // arrives on the parent session and names the dying session in
            // its params; route it to that session.
            let key = match method {
                "Target.detachedFromTarget" => params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(session_id),
                _ => session_id,
            };
            if let Some(key) = key {
                inner.critical_queue(&key).push(event);
            } else {
                tracing::debug!(method = %method, "cdp: browser-level critical event has no session");
            }
        }
        EventClass::Observation => {
            // A full broadcast channel drops for the slowest receiver only;
            // the send error here means "no subscribers", which is not an
            // error.
            let _ = inner.events.send(event);
        }
    }
}

/// Await a CDP command with a small convenience timeout derived from a
/// duration (used by tests and operational helpers).
pub async fn send_with_timeout(
    client: &CdpClient,
    session: Option<&str>,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, BrowserError> {
    let deadline = Deadline::at(
        faktor_core::time::Clock::now_ms(&faktor_core::time::SystemClock)
            + timeout.as_millis().min(i64::MAX as u128) as i64,
    );
    client
        .send(session, method, params, deadline, &CancellationToken::new())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner(config: CdpConfig) -> Arc<CdpInner> {
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        Arc::new(CdpInner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            events,
            session_critical: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            closed_reason: Mutex::new(None),
            writer: AsyncMutex::new(None),
            reader: AsyncMutex::new(None),
            config,
        })
    }

    #[test]
    fn overflow_hook_fires_from_push_without_a_consumer() {
        // The page's fail_stream must be reachable the moment overflow
        // latches: a pump parked in a per-event handler must never delay the
        // typed lag. The hook is called with the exact skipped count, once.
        let queue = CriticalQueue::new(2);
        let seen: std::sync::Arc<std::sync::Mutex<Vec<u64>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        queue.set_overflow_hook(std::sync::Arc::new(move |skipped| {
            sink.lock().unwrap().push(skipped);
        }));
        for index in 0..5u64 {
            queue.push(CdpEvent {
                session_id: Some("s1".into()),
                method: "Fetch.requestPaused".into(),
                params: serde_json::json!({ "requestId": format!("r{index}") }),
            });
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![3],
            "overflow on the 3rd push (capacity 2) must call the hook exactly once"
        );
        // Later pushes are already failed: no additional hook call.
        queue.push(CdpEvent {
            session_id: Some("s1".into()),
            method: "Fetch.requestPaused".into(),
            params: serde_json::json!({ "requestId": "late" }),
        });
        assert_eq!(*seen.lock().unwrap(), vec![3]);
    }

    #[test]
    fn cdp_config_defaults_are_bounded() {
        let config = CdpConfig::default();
        assert!(config.max_message_bytes > 0);
        assert!(config.event_capacity > 0);
        assert!(config.critical_event_capacity > 0);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn cdp_config_refuses_zero_and_absurd_capacities() {
        for bad in [
            CdpConfig {
                max_message_bytes: 0,
                ..CdpConfig::default()
            },
            CdpConfig {
                max_message_bytes: CDP_MAX_MESSAGE_CEILING_BYTES + 1,
                ..CdpConfig::default()
            },
            CdpConfig {
                event_capacity: 0,
                ..CdpConfig::default()
            },
            CdpConfig {
                event_capacity: CDP_MAX_CAPACITY_CEILING + 1,
                ..CdpConfig::default()
            },
            CdpConfig {
                critical_event_capacity: 0,
                ..CdpConfig::default()
            },
            CdpConfig {
                critical_event_capacity: CDP_MAX_CAPACITY_CEILING + 1,
                ..CdpConfig::default()
            },
        ] {
            let error = bad.validate().expect_err("must be a configuration error");
            assert_eq!(error.code(), "invalid_config", "{bad:?}");
        }
    }

    #[test]
    fn event_classification_separates_critical_from_observation() {
        for critical in [
            "Fetch.requestPaused",
            "Target.targetCrashed",
            "Target.detachedFromTarget",
            "Inspector.detached",
            "Page.frameNavigated",
            "Page.domContentEventFired",
            "Page.loadEventFired",
        ] {
            assert_eq!(classify_event(critical), EventClass::Critical, "{critical}");
        }
        for observation in [
            "Network.requestWillBeSent",
            "Network.responseReceived",
            "Browser.downloadWillBegin",
            "Runtime.exceptionThrown",
            "Unknown.method",
        ] {
            assert_eq!(
                classify_event(observation),
                EventClass::Observation,
                "{observation}"
            );
        }
    }

    #[tokio::test]
    async fn critical_queue_overflow_latches_and_reports_the_lag() {
        let queue = CriticalQueue::new(2);
        queue.push(CdpEvent {
            session_id: Some("s1".into()),
            method: "Fetch.requestPaused".into(),
            params: json!({"requestId": "r1"}),
        });
        queue.push(CdpEvent {
            session_id: Some("s1".into()),
            method: "Fetch.requestPaused".into(),
            params: json!({"requestId": "r2"}),
        });
        // Third event: overflow abandons both queued events plus itself.
        queue.push(CdpEvent {
            session_id: Some("s1".into()),
            method: "Fetch.requestPaused".into(),
            params: json!({"requestId": "r3"}),
        });
        let error = queue.recv().await.expect_err("overflow must latch");
        assert_eq!(error, EventStreamError::Lagged { skipped: 3 });
        // The lag is sticky: the stream never silently continues.
        assert_eq!(
            queue.recv().await.expect_err("sticky lag"),
            EventStreamError::Lagged { skipped: 3 }
        );
    }

    #[tokio::test]
    async fn critical_events_never_ride_the_lossy_broadcast() {
        let config = CdpConfig {
            event_capacity: 8,
            critical_event_capacity: 4,
            ..CdpConfig::default()
        };
        let inner = inner(config);
        let queue = inner.critical_queue("s1");
        let mut observations = inner.events.subscribe();
        route_message(
            json!({
                "method": "Fetch.requestPaused",
                "sessionId": "s1",
                "params": {"requestId": "r1"}
            }),
            &inner,
        );
        route_message(
            json!({
                "method": "Network.requestWillBeSent",
                "sessionId": "s1",
                "params": {"requestId": "r1"}
            }),
            &inner,
        );
        let critical = queue.recv().await.expect("critical event delivered");
        assert_eq!(critical.method, "Fetch.requestPaused");
        let observation = observations.recv().await.expect("observation delivered");
        assert_eq!(observation.method, "Network.requestWillBeSent");
        // The critical event is not on the broadcast: the next message is the
        // only observation event.
        route_message(
            json!({
                "method": "Network.loadingFinished",
                "sessionId": "s1",
                "params": {"requestId": "r1"}
            }),
            &inner,
        );
        let next = observations.recv().await.expect("second observation");
        assert_eq!(next.method, "Network.loadingFinished");
    }

    #[tokio::test]
    async fn detached_target_is_routed_to_the_dying_session() {
        let config = CdpConfig {
            critical_event_capacity: 4,
            ..CdpConfig::default()
        };
        let inner = inner(config);
        let queue = inner.critical_queue("dying");
        route_message(
            json!({
                "method": "Target.detachedFromTarget",
                "params": {"sessionId": "dying", "targetId": "t1"}
            }),
            &inner,
        );
        let event = queue.recv().await.expect("routed by params.sessionId");
        assert_eq!(event.method, "Target.detachedFromTarget");
    }

    #[tokio::test]
    async fn oversized_inbound_messages_close_the_transport_at_the_ws_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let oversized = "x".repeat(4096);
            let _ = ws.send(Message::text(oversized)).await;
            // Keep the socket open so the client-side cap (not EOF) is what
            // closes the connection.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let config = CdpConfig {
            max_message_bytes: 1024,
            ..CdpConfig::default()
        };
        let client = CdpClient::connect(
            &format!("ws://{addr}/devtools/browser/limit-test"),
            &CancellationToken::new(),
            Deadline::now_plus(&faktor_core::time::SystemClock, 5_000),
            config,
        )
        .await
        .expect("handshake succeeds");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !client.is_closed() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            client.is_closed(),
            "a message over the WS-layer limit must close the transport"
        );
    }

    #[tokio::test]
    async fn connect_refuses_hostile_endpoints_before_dialing() {
        for hostile in [
            "wss://127.0.0.1:9222/devtools/browser/x",
            "ws://attacker.example:9222/devtools/browser/x",
            "ws://localhost:9222/devtools/browser/x",
            "ws://user:pass@127.0.0.1:9222/devtools/browser/x",
            "ws://127.0.0.1:0/devtools/browser/x",
            "ws://127.0.0.1:9222/devtools/page/x",
            "ws://127.0.0.1:9222/devtools/browser/x?y=1",
        ] {
            let error = CdpClient::connect(
                hostile,
                &CancellationToken::new(),
                Deadline::now_plus(&faktor_core::time::SystemClock, 1_000),
                CdpConfig::default(),
            )
            .await
            .expect_err("hostile endpoint must be refused");
            assert!(matches!(
                error,
                BrowserError::Cdp { .. } | BrowserError::InvalidConfig { .. }
            ));
        }
        // Zero/absurd caps are configuration errors, not a dial.
        let error = CdpClient::connect(
            "ws://127.0.0.1:9222/devtools/browser/x",
            &CancellationToken::new(),
            Deadline::now_plus(&faktor_core::time::SystemClock, 1_000),
            CdpConfig {
                max_message_bytes: 0,
                ..CdpConfig::default()
            },
        )
        .await
        .expect_err("zero cap must be refused");
        assert_eq!(error.code(), "invalid_config");
    }
}
