//! Adversarial test support and suites.
//!
//! Everything here is test-only. The scripted transport implements the
//! production `HttpTransport` seam so the runtime can be driven through
//! canned statuses, headers, bodies, gate-controlled blocking and injected
//! failures without ever touching the network.

pub mod cache;
pub mod coalesce;
pub mod health;
pub mod http;
pub mod planner;
pub mod quota;
pub mod retry;
pub mod scan;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::TestClock;
use faktor_provider::egress::{EgressError, HttpTransport};
use futures::future::BoxFuture;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Request, Response, ResponseBuilderExt};

use crate::AcquireCtx;

/// One scripted transport response (or an injected transport failure).
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    /// The status to return.
    pub status: u16,
    /// The headers to return.
    pub headers: Vec<(String, String)>,
    /// The body to return.
    pub body: Vec<u8>,
    /// When set, the transport fails with this egress error instead.
    pub error: Option<EgressError>,
}

impl ScriptedResponse {
    /// A response with the given status and text body.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            error: None,
        }
    }

    /// A 200 JSON response.
    pub fn json(body: impl Into<Vec<u8>>) -> Self {
        Self::new(200, body).header("content-type", "application/json")
    }

    /// A response with one extra header.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// An injected transport failure.
    pub fn failure(error: EgressError) -> Self {
        Self {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            error: Some(error),
        }
    }
}

/// One recorded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRequest {
    /// The HTTP method.
    pub method: String,
    /// The full URL.
    pub url: String,
    /// The request headers.
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: Option<Vec<u8>>,
}

impl RecordedRequest {
    /// The first value of a header (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A gate that lets a test hold a request inside the transport until it
/// decides to release it.
pub struct Gate {
    /// How many requests have entered the transport.
    pub entered: AtomicUsize,
    /// Notified when a request enters the transport.
    pub entered_notify: tokio::sync::Notify,
    /// Requests wait for a permit here; `add_permits` releases them.
    pub release: tokio::sync::Semaphore,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            entered: AtomicUsize::new(0),
            entered_notify: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl Gate {
    /// A closed gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait until at least `count` requests have entered.
    pub async fn wait_entered(&self, count: usize) {
        while self.entered.load(Ordering::SeqCst) < count {
            self.entered_notify.notified().await;
        }
    }

    /// Release one blocked request.
    pub fn release_one(&self) {
        self.release.add_permits(1);
    }

    /// Release every blocked request.
    pub fn release_all(&self) {
        self.release.add_permits(usize::MAX >> 4);
    }
}

/// A transport double: scripted responses, recorded requests, optional gate.
pub struct ScriptedTransport {
    responses: Mutex<VecDeque<ScriptedResponse>>,
    requests: Mutex<Vec<RecordedRequest>>,
    gate: Option<Arc<Gate>>,
}

impl ScriptedTransport {
    /// A transport that answers with the given script, in order.
    pub fn new(responses: impl IntoIterator<Item = ScriptedResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            gate: None,
        }
    }

    /// A transport whose every request blocks on `gate` before answering.
    pub fn gated(responses: impl IntoIterator<Item = ScriptedResponse>, gate: Arc<Gate>) -> Self {
        Self {
            gate: Some(gate),
            ..Self::new(responses)
        }
    }

    /// How many requests were executed.
    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// The recorded requests, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The most recent recorded request.
    pub fn last_request(&self) -> Option<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .last()
            .cloned()
    }

    fn record(&self, request: &Request) {
        let headers = request
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = request
            .body()
            .and_then(|body| body.as_bytes())
            .map(|bytes| bytes.to_vec());
        self.requests
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(RecordedRequest {
                method: request.method().to_string(),
                url: request.url().to_string(),
                headers,
                body,
            });
    }
}

impl HttpTransport for ScriptedTransport {
    fn execute(&self, request: Request) -> BoxFuture<'_, Result<Response, EgressError>> {
        self.record(&request);
        let scripted = self
            .responses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pop_front()
            .unwrap_or_else(|| {
                ScriptedResponse::failure(EgressError::Transport(
                    "script exhausted: unexpected request".into(),
                ))
            });
        let gate = self.gate.clone();
        let url = request.url().clone();
        Box::pin(async move {
            if let Some(gate) = gate {
                gate.entered.fetch_add(1, Ordering::SeqCst);
                gate.entered_notify.notify_waiters();
                let _permit = gate
                    .release
                    .acquire()
                    .await
                    .map_err(|_| EgressError::Transport("gate closed".into()))?;
            }
            if let Some(error) = scripted.error {
                return Err(error);
            }
            let mut builder = ::http::Response::builder().status(scripted.status).url(url);
            for (name, value) in &scripted.headers {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| EgressError::Build(e.to_string()))?;
                let value =
                    HeaderValue::from_str(value).map_err(|e| EgressError::Build(e.to_string()))?;
                builder = builder.header(name, value);
            }
            let response = builder
                .body(reqwest::Body::from(scripted.body))
                .map_err(|e| EgressError::Build(e.to_string()))?;
            Ok(Response::from(response))
        })
    }
}

/// A context over a scripted transport and a controllable clock.
pub fn ctx_for(transport: Arc<ScriptedTransport>) -> (AcquireCtx, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000_000));
    let ctx = AcquireCtx::new(transport, clock.clone(), CancellationToken::new());
    (ctx, clock)
}

/// A plain URL used across the suites.
pub const TEST_URL: &str = "https://example.invalid/data";
