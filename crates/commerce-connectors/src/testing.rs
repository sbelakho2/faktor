//! Offline replay support (spec §16: fixtures replayable offline in CI).
//!
//! These are the seams the test suite (and the acquire runtime's own
//! certification suites) use to run connectors without a network:
//!
//! * [`FixtureTransport`] replays queued canned responses and records every
//!   request it received — the request log is what the adversarial tests
//!   assert on (which endpoint was called, whether a credential was used,
//!   whether the browser was consulted);
//! * [`MapCredentials`] injects credential values by env-var *name*;
//! * [`RecordingBrowser`] is a `BrowserFallback` that records calls instead
//!   of launching anything;
//! * [`fixture`] loads one sanitized JSON fixture from `fixtures/`.
//!
//! Nothing here performs I/O beyond reading a fixture file from the crate
//! directory.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{SourceError, SourceId};

use crate::browser::{BrowserFallback, FallbackObservation};
use crate::config::ConfigError;
use crate::context::AcquireCtx;
use crate::http::{Header, HttpRequest, HttpResponse, HttpTransport, TransportError};
use crate::secrets::{CredentialProvider, SecretString};

/// A transport that always fails: the safest default for a context that
/// must not reach the network.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTransport;

#[async_trait]
impl HttpTransport for NoopTransport {
    async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
        Err(TransportError::EgressUnavailable)
    }
}

/// One canned response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CannedResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CannedResponse {
    /// A response with the given status and body.
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body,
        }
    }

    /// A `200 OK` JSON response.
    pub fn json(body: &str) -> Self {
        Self::new(200, body.as_bytes().to_vec())
    }

    /// A response with the given status and body.
    pub fn with_status(status: u16, body: &str) -> Self {
        Self::new(status, body.as_bytes().to_vec())
    }

    /// Add one response header (validated by [`Header`]).
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        if let Ok(header) = Header::new(name, value) {
            self.headers
                .push((header.name().to_string(), header.value().to_string()));
        }
        self
    }

    /// The response status.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The response body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    fn build(&self) -> Result<HttpResponse, TransportError> {
        let headers = self
            .headers
            .iter()
            .filter_map(|(name, value)| Header::new(name, value).ok())
            .collect();
        HttpResponse::new(self.status, headers, self.body.clone())
            .map_err(|_| TransportError::ResponseTooLarge)
    }
}

#[derive(Default)]
struct FixtureState {
    queue: VecDeque<Result<CannedResponse, TransportError>>,
    requests: Vec<HttpRequest>,
}

/// A replay transport: pops one queued response per request and records the
/// request. An empty queue yields `EgressUnavailable` (fail closed).
#[derive(Default)]
pub struct FixtureTransport {
    state: Mutex<FixtureState>,
}

impl FixtureTransport {
    /// An empty transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue one response.
    pub fn enqueue(self, response: CannedResponse) -> Self {
        self.push(response);
        self
    }

    /// Queue one transport failure.
    pub fn enqueue_error(self, error: TransportError) -> Self {
        self.push_error(error);
        self
    }

    /// Append one response without consuming the transport (shared behind an
    /// `Arc`).
    pub fn push(&self, response: CannedResponse) {
        self.push_item(Ok(response));
    }

    /// Append one transport failure without consuming the transport.
    pub fn push_error(&self, error: TransportError) {
        self.push_item(Err(error));
    }

    fn push_item(&self, item: Result<CannedResponse, TransportError>) {
        self.lock().queue.push_back(item);
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.lock().requests.clone()
    }

    /// How many requests were received.
    pub fn request_count(&self) -> usize {
        self.lock().requests.len()
    }

    /// The URL of the request at `index`, when it exists.
    pub fn request_url(&self, index: usize) -> Option<String> {
        self.lock()
            .requests
            .get(index)
            .map(|request| request.url().as_str().to_string())
    }

    /// The header value of the request at `index`.
    pub fn request_header(&self, index: usize, name: &str) -> Option<String> {
        self.lock().requests.get(index).and_then(|request| {
            request
                .headers()
                .iter()
                .find(|header| header.name().eq_ignore_ascii_case(name))
                .map(|header| header.value().to_string())
        })
    }

    /// The body of the request at `index`.
    pub fn request_body(&self, index: usize) -> Option<String> {
        self.lock().requests.get(index).and_then(|request| {
            request
                .body()
                .map(|body| String::from_utf8_lossy(body).to_string())
        })
    }

    /// Every recorded request rendered with redaction (Debug form).
    pub fn rendered_requests(&self) -> String {
        self.lock()
            .requests
            .iter()
            .map(|request| format!("{request:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FixtureState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl HttpTransport for FixtureTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut state = self.lock();
        state.requests.push(request);
        match state.queue.pop_front() {
            Some(Ok(response)) => response.build(),
            Some(Err(error)) => Err(error),
            None => Err(TransportError::EgressUnavailable),
        }
    }
}

/// A credential provider backed by a map (tests; never the production
/// source).
#[derive(Debug, Clone, Default)]
pub struct MapCredentials {
    values: BTreeMap<String, String>,
}

impl MapCredentials {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert one name → value pair.
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.values.insert(name.to_string(), value.to_string());
        self
    }
}

impl CredentialProvider for MapCredentials {
    fn resolve(&self, env_name: &str) -> Result<SecretString, ConfigError> {
        match self.values.get(env_name) {
            Some(value) if !value.trim().is_empty() => Ok(SecretString::new(value.clone())),
            Some(_) => Err(ConfigError::EmptyCredential {
                env_name: env_name.to_string(),
            }),
            None => Err(ConfigError::MissingCredential {
                env_name: env_name.to_string(),
            }),
        }
    }
}

/// A browser fallback that records calls and returns a canned observation.
#[derive(Debug, Default)]
pub struct RecordingBrowser {
    calls: Mutex<Vec<String>>,
    observation: Mutex<Option<FallbackObservation>>,
    fail: bool,
}

impl RecordingBrowser {
    /// A recording browser returning an empty observation.
    pub fn new() -> Self {
        Self::default()
    }

    /// A recording browser returning `observation`.
    pub fn returning(observation: FallbackObservation) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            observation: Mutex::new(Some(observation)),
            fail: false,
        }
    }

    /// A recording browser that fails with `BrowserUnavailable`.
    pub fn failing() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            observation: Mutex::new(None),
            fail: true,
        }
    }

    /// How many times the browser was consulted.
    pub fn calls(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// The URLs the browser was asked for.
    pub fn requested_urls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl BrowserFallback for RecordingBrowser {
    async fn observe(
        &self,
        _ctx: &AcquireCtx,
        _source: &SourceId,
        url: &CanonicalUrl,
        _wanted: &[&'static str],
    ) -> Result<FallbackObservation, SourceError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.as_str().to_string());
        if self.fail {
            return Err(SourceError::BrowserUnavailable);
        }
        let observation = self
            .observation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        observation.ok_or(SourceError::ExtractionIncomplete)
    }
}

/// Load one fixture file from `crates/commerce-connectors/fixtures/`.
pub fn fixture(relative: &str) -> Result<String, std::io::Error> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(relative);
    std::fs::read_to_string(path)
}

/// Build a bounded text value for fixtures (panics only in test setup).
pub fn text<const MAX: usize>(raw: &str) -> Text<MAX> {
    Text::<MAX>::new(raw).expect("fixture text is valid")
}
