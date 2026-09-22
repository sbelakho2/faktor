//! CDP transport: one WebSocket session to Chromium's DevTools endpoint.
//!
//! The client correlates command responses by `id`, publishes protocol
//! events on a bounded broadcast channel, enforces a per-command deadline, a
//! message-size bound, and observes caller cancellation. A closed socket
//! fails every in-flight command typed (never a hang); the supervisor's
//! registry remains the authority on whether the child actually died.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::Deadline;

use crate::error::BrowserError;
use crate::timeutil::deadline_instant;

/// CDP client bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpConfig {
    /// Maximum accepted inbound message size (CDP events/bodies).
    pub max_message_bytes: usize,
    /// Bounded event fan-out capacity; a lagging consumer is told it lagged.
    pub event_capacity: usize,
}

impl Default for CdpConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: 8 * 1024 * 1024,
            event_capacity: 2048,
        }
    }
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
    /// honored during the handshake.
    pub async fn connect(
        ws_url: &str,
        cancel: &CancellationToken,
        deadline: Deadline,
        config: CdpConfig,
    ) -> Result<Self, BrowserError> {
        let (events, _) = broadcast::channel(config.event_capacity.max(1));
        let inner = Arc::new(CdpInner {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            events,
            closed: AtomicBool::new(false),
            closed_reason: Mutex::new(None),
            writer: AsyncMutex::new(None),
            reader: AsyncMutex::new(None),
            config: config.clone(),
        });
        let connect = connect_async(ws_url);
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

    /// Subscribe to CDP events. A lagging subscriber observes `Lagged` and
    /// must resynchronize (bounded fan-out, never unbounded buffering).
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.inner.events.subscribe()
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
    let event = CdpEvent {
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        method: method.to_string(),
        params: value.get("params").cloned().unwrap_or(Value::Null),
    };
    // A full broadcast channel drops for the slowest receiver only; the
    // send error here means "no subscribers", which is not an error.
    let _ = inner.events.send(event);
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

    #[test]
    fn cdp_config_defaults_are_bounded() {
        let config = CdpConfig::default();
        assert!(config.max_message_bytes > 0);
        assert!(config.event_capacity > 0);
    }
}
