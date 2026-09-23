//! Page operations: navigation, DOM access, JS evaluation, lifecycle,
//! cancellation, screenshots, and generic verification detection.
//!
//! A [`Page`] owns one CDP target session and a bounded network tracker. All
//! operations take an explicit deadline and cancellation token; cancelling a
//! navigation stops loading and releases the page while leaving the browser
//! process (and its profile) healthy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::Deadline;

use crate::capture::{bound_bytes, bound_text, CaptureLimits, CapturedBytes, CapturedText};
use crate::cdp::{CdpClient, CdpEvent};
use crate::download::DownloadPolicy;
use crate::error::{BrowserError, VerificationKind};
use crate::interception::Interceptor;
use crate::network::{BodyCapturer, CapturedBody, NetworkLimits, NetworkNotice, NetworkTracker};
use crate::timeutil::{deadline_in, deadline_instant};

/// Lifecycle of a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    Open,
    Navigating,
    Closed,
    Crashed,
}

/// Document lifecycle progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Idle,
    Navigating,
    DomContentLoaded,
    Loaded,
    Failed,
}

/// Result of one navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationOutcome {
    pub url: String,
    pub status: Option<i64>,
    pub error_text: Option<String>,
    pub lifecycle: Lifecycle,
}

/// A detected human-verification signal. Detection is generic (DOM
/// landmarks + page text), never site knowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationSignal {
    LoginForm,
    Captcha,
    SecuritySlider,
    AccessDenied,
    RateLimit,
    Interstitial,
}

impl VerificationSignal {
    pub fn kind(self) -> VerificationKind {
        match self {
            VerificationSignal::LoginForm => VerificationKind::LoginForm,
            VerificationSignal::Captcha => VerificationKind::Captcha,
            VerificationSignal::SecuritySlider => VerificationKind::SecuritySlider,
            VerificationSignal::AccessDenied => VerificationKind::AccessDenied,
            VerificationSignal::RateLimit => VerificationKind::RateLimit,
            VerificationSignal::Interstitial => VerificationKind::Interstitial,
        }
    }

    /// The typed error the caller observes for this signal.
    pub fn error(self) -> BrowserError {
        match self {
            VerificationSignal::LoginForm => BrowserError::AuthenticationRequired,
            VerificationSignal::RateLimit => BrowserError::RateLimited {
                retry_after_ms: None,
            },
            other => BrowserError::VerificationRequired { kind: other.kind() },
        }
    }
}

/// The probe the page runs to classify a verification interstitial. It is a
/// single generic expression: password inputs, CAPTCHA/slider containers,
/// access-denied text and rate-limit text. It carries no site knowledge.
pub const VERIFICATION_PROBE_JS: &str = r#"
/* faktor-verify-probe */
(() => {
  const q = (s) => { try { return !!document.querySelector(s); } catch (e) { return false; } };
  const text = ((document.body && document.body.innerText) || '').slice(0, 8192).toLowerCase();
  return JSON.stringify({
    login_form: q('input[type="password"]') || q('form[action*="login" i]') || q('form[action*="signin" i]'),
    captcha: q('iframe[src*="captcha" i], iframe[src*="recaptcha" i], iframe[src*="hcaptcha" i], [class*="captcha" i], [id*="captcha" i], [class*="geetest" i]'),
    security_slider: q('[class*="slider" i][class*="verify" i], [class*="nc_iconfont" i], [id^="nc_" i][class*="btn" i], [class*="slide-verify" i]'),
    access_denied: /access denied|access forbidden|403 forbidden|not authorised|not authorized/.test(text),
    rate_limited: /rate limit|too many requests|try again later|访问频繁|请求过于频繁/.test(text),
    interstitial: q('[class*="challenge" i], [id*="challenge" i], [class*="verify" i], [class*="verification" i]')
  });
})()
"#;

/// Host-side hooks a page calls back into (implemented by the manager's
/// browser instance).
pub trait PageHost: Send + Sync {
    /// The page closed or was released: drop it from the profile's page set.
    fn release_page(&self, target_id: &str);
    /// A verification signal was detected: stop the profile's automated
    /// work.
    fn stop_automation(&self, signal: VerificationSignal);
}

pub(crate) struct PageInner {
    target_id: String,
    session_id: String,
    client: CdpClient,
    tracker: Mutex<NetworkTracker>,
    interceptor: Interceptor,
    downloads: DownloadPolicy,
    capture: CaptureLimits,
    state: Mutex<PageState>,
    lifecycle: watch::Sender<Lifecycle>,
    last_navigation_error: Mutex<Option<String>>,
    last_download_error: Mutex<Option<BrowserError>>,
    closed: AtomicBool,
    host: Weak<dyn PageHost>,
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Backstop for abandoned pages: a cancelled capture whose future (and thus
/// its last [`Page`] handle) is dropped without [`Page::close`] must still
/// release the profile slot synchronously, and must not keep its CDP target
/// alive. The manager's page set holds weak handles, so this `Drop` is what
/// makes the slot release automatic.
impl Drop for PageInner {
    fn drop(&mut self) {
        let was_closed = self.closed.swap(true, Ordering::SeqCst);
        let pump = self
            .pump
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pump) = pump {
            pump.abort();
        }
        if let Some(host) = self.host.upgrade() {
            host.release_page(&self.target_id);
        }
        if was_closed || self.client.is_closed() {
            return;
        }
        // Best-effort target teardown. `Drop` cannot await; when no runtime
        // is entered (process teardown) the browser child is killed by the
        // manager's owner-scoped teardown anyway.
        let client = self.client.clone();
        let target_id = self.target_id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = client
                    .send(
                        None,
                        "Target.closeTarget",
                        json!({ "targetId": target_id }),
                        deadline_in(2_000),
                        &CancellationToken::new(),
                    )
                    .await;
            });
        }
    }
}

/// A page handle. Cloning shares the same target session.
#[derive(Clone)]
pub struct Page {
    inner: Arc<PageInner>,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("target_id", &self.inner.target_id)
            .field("state", &self.state())
            .finish()
    }
}

impl Page {
    /// Build a page around an attached CDP session and start its event pump.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach(
        client: CdpClient,
        target_id: String,
        session_id: String,
        interceptor: Interceptor,
        downloads: DownloadPolicy,
        capture: CaptureLimits,
        network: NetworkLimits,
        host: Weak<dyn PageHost>,
    ) -> Self {
        let (lifecycle, _) = watch::channel(Lifecycle::Idle);
        let inner = Arc::new(PageInner {
            target_id,
            session_id,
            client,
            tracker: Mutex::new(NetworkTracker::new(network)),
            interceptor,
            downloads,
            capture,
            state: Mutex::new(PageState::Open),
            lifecycle,
            last_navigation_error: Mutex::new(None),
            last_download_error: Mutex::new(None),
            closed: AtomicBool::new(false),
            host,
            pump: Mutex::new(None),
        });
        let pump_inner = Arc::downgrade(&inner);
        // Subscribe BEFORE spawning the pump: a subscription created inside
        // the task races the first navigation, and a broadcast send with no
        // subscriber is dropped.
        let events = inner.client.subscribe();
        let handle = tokio::spawn(pump_events(pump_inner, events));
        *inner.pump.lock().unwrap() = Some(handle);
        Self { inner }
    }

    pub fn target_id(&self) -> &str {
        &self.inner.target_id
    }

    /// A weak handle to the same page session. The manager's page set holds
    /// weak handles, so dropping the caller's last [`Page`] releases the
    /// profile slot even when the caller never calls [`Page::close`]
    /// (abandoned/cancelled captures).
    pub(crate) fn downgrade(&self) -> Weak<PageInner> {
        Arc::downgrade(&self.inner)
    }

    /// Upgrade a weak handle back into a page, when it is still alive.
    pub(crate) fn upgrade(weak: &Weak<PageInner>) -> Option<Page> {
        weak.upgrade().map(|inner| Page { inner })
    }

    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    pub fn state(&self) -> PageState {
        *self.inner.state.lock().unwrap()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    pub fn lifecycle(&self) -> Lifecycle {
        *self.inner.lifecycle.borrow()
    }

    /// Bounded snapshot of observed network requests.
    pub fn network(&self) -> Vec<crate::network::NetworkRequest> {
        self.inner.tracker.lock().unwrap().requests()
    }

    pub fn interceptor_stats(&self) -> crate::interception::InterceptionStats {
        self.inner.interceptor.stats()
    }

    /// A download denial observed for this page, if any (typed).
    pub fn last_download_error(&self) -> Option<BrowserError> {
        self.inner.last_download_error.lock().unwrap().clone()
    }

    /// Navigate and wait for the load lifecycle. Cancellation stops loading,
    /// releases the page, and returns `Cancelled`; the browser process stays
    /// healthy.
    pub async fn navigate(
        &self,
        url: &str,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<NavigationOutcome, BrowserError> {
        self.ensure_open()?;
        self.set_state(PageState::Navigating);
        *self.inner.last_navigation_error.lock().unwrap() = None;
        let result = self
            .inner
            .client
            .send(
                Some(&self.inner.session_id),
                "Page.navigate",
                json!({ "url": url }),
                deadline,
                cancel,
            )
            .await;
        let value = match result {
            Ok(value) => value,
            Err(BrowserError::Cancelled) => {
                self.abort_navigation(cancel).await;
                return Err(BrowserError::Cancelled);
            }
            Err(error) => {
                self.set_state(PageState::Open);
                return Err(self.transport_error(error));
            }
        };
        let error_text = value
            .get("errorText")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(error_text) = &error_text {
            *self.inner.last_navigation_error.lock().unwrap() = Some(error_text.clone());
        }
        let mut lifecycle = self.inner.lifecycle.subscribe();
        loop {
            if *lifecycle.borrow_and_update() == Lifecycle::Loaded {
                break;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.abort_navigation(cancel).await;
                    return Err(BrowserError::Cancelled);
                }
                changed = lifecycle.changed() => {
                    if changed.is_err() {
                        self.set_state(PageState::Crashed);
                        return Err(self.closed_error());
                    }
                }
                _ = tokio::time::sleep_until(deadline_instant(deadline)) => {
                    self.abort_navigation(cancel).await;
                    return Err(BrowserError::Deadline { detail: format!("navigation to {url}") });
                }
            }
        }
        self.set_state(PageState::Open);
        let final_url = self
            .evaluate_inner("location.href", deadline, cancel)
            .await
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| url.to_string());
        let status = self.document_status();
        Ok(NavigationOutcome {
            url: final_url,
            status,
            error_text,
            lifecycle: Lifecycle::Loaded,
        })
    }

    /// Stop an in-flight navigation without killing the browser.
    pub async fn stop_navigation(&self, cancel: &CancellationToken) -> Result<(), BrowserError> {
        self.inner
            .client
            .send(
                Some(&self.inner.session_id),
                "Page.stopLoading",
                json!({}),
                deadline_in(3_000),
                cancel,
            )
            .await
            .map(|_| ())
    }

    /// Evaluate a JS expression, returning its JSON value under a byte bound.
    pub async fn evaluate(
        &self,
        expression: &str,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Value, BrowserError> {
        self.ensure_open()?;
        self.evaluate_inner(expression, deadline, cancel).await
    }

    async fn evaluate_inner(
        &self,
        expression: &str,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Value, BrowserError> {
        let result = self
            .inner
            .client
            .send(
                Some(&self.inner.session_id),
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true
                }),
                deadline,
                cancel,
            )
            .await
            .map_err(|error| self.transport_error(error))?;
        if result.get("exceptionDetails").is_some() {
            let detail = result["exceptionDetails"]
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("javascript exception");
            return Err(BrowserError::Cdp {
                detail: format!("evaluate raised: {detail}"),
            });
        }
        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null);
        let rendered = value.to_string();
        if rendered.len() > self.inner.capture.max_text_bytes {
            return Err(BrowserError::ResponseTooLarge {
                limit_bytes: self.inner.capture.max_text_bytes,
                observed_bytes: Some(rendered.len()),
            });
        }
        Ok(value)
    }

    /// The main document's outer HTML, bounded.
    pub async fn document_html(
        &self,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<CapturedText, BrowserError> {
        let value = self
            .evaluate_inner(
                "document.documentElement ? document.documentElement.outerHTML : ''",
                deadline,
                cancel,
            )
            .await?;
        let raw = value.as_str().unwrap_or_default();
        Ok(bound_text(raw, self.inner.capture.max_dom_bytes))
    }

    /// Run the generic verification probe. On detection the profile's
    /// automated work is stopped and the signal returned.
    pub async fn detect_verification(
        &self,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Option<VerificationSignal>, BrowserError> {
        let value = self
            .evaluate_inner(VERIFICATION_PROBE_JS, deadline, cancel)
            .await?;
        let Some(raw) = value.as_str() else {
            return Ok(None);
        };
        let parsed: Value = serde_json::from_str(raw).map_err(|e| BrowserError::Cdp {
            detail: format!("verification probe returned malformed json: {e}"),
        })?;
        let signal = classify_probe(&parsed);
        if let Some(signal) = signal {
            self.stop_automation(signal);
        }
        Ok(signal)
    }

    /// Probe and convert a detection into the typed error, or `Ok(())`.
    pub async fn guard_verification(
        &self,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<(), BrowserError> {
        match self.detect_verification(deadline, cancel).await? {
            Some(signal) => Err(signal.error()),
            None => Ok(()),
        }
    }

    /// Screenshot on explicit request only. The decoded image is bounded.
    pub async fn screenshot(
        &self,
        format: &str,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<CapturedBytes, BrowserError> {
        self.ensure_open()?;
        if !matches!(format, "png" | "jpeg" | "webp") {
            return Err(BrowserError::invalid_config(format!(
                "unsupported screenshot format {format:?}"
            )));
        }
        let result = self
            .inner
            .client
            .send(
                Some(&self.inner.session_id),
                "Page.captureScreenshot",
                json!({ "format": format }),
                deadline,
                cancel,
            )
            .await?;
        let data = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::cdp("captureScreenshot returned no data"))?;
        let capture = &self.inner.capture;
        let decoded = crate::capture::decode_cdp_body(
            data,
            true,
            capture.max_screenshot_bytes,
            capture.max_screenshot_bytes,
        )?;
        Ok(bound_bytes(decoded.bytes, capture.max_screenshot_bytes))
    }

    /// Capture one response body (explicit request only), bounded.
    pub async fn capture_body(
        &self,
        request_id: &str,
        cap_bytes: usize,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<CapturedBody, BrowserError> {
        let url = self
            .inner
            .tracker
            .lock()
            .unwrap()
            .get(request_id)
            .map(|record| record.url);
        let capturer = BodyCapturer::new(
            self.inner.client.clone(),
            self.inner.session_id.clone(),
            NetworkLimits {
                max_records: self.inner.capture.max_network_records,
                max_body_bytes: self.inner.capture.max_body_bytes,
                hard_max_body_bytes: self.inner.capture.hard_max_body_bytes,
            },
        );
        capturer
            .capture(request_id, url.as_deref(), cap_bytes, deadline, cancel)
            .await
    }

    /// Close the target and release the page slot. Idempotent.
    pub async fn close(&self) -> Result<(), BrowserError> {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.set_state(PageState::Closed);
        if let Some(pump) = self.inner.pump.lock().unwrap().take() {
            pump.abort();
        }
        let result = self
            .inner
            .client
            .send(
                None,
                "Target.closeTarget",
                json!({ "targetId": self.inner.target_id }),
                deadline_in(3_000),
                &CancellationToken::new(),
            )
            .await;
        if let Some(host) = self.inner.host.upgrade() {
            host.release_page(&self.inner.target_id);
        }
        result.map(|_| ())
    }

    /// Called by the pump when the CDP session dies (browser crash).
    pub(crate) fn mark_crashed(&self) {
        if !self.inner.closed.swap(true, Ordering::SeqCst) {
            self.set_state(PageState::Crashed);
            let _ = self.inner.lifecycle.send(Lifecycle::Failed);
            if let Some(host) = self.inner.host.upgrade() {
                host.release_page(&self.inner.target_id);
            }
        }
    }

    fn stop_automation(&self, signal: VerificationSignal) {
        if let Some(host) = self.inner.host.upgrade() {
            host.stop_automation(signal);
        }
    }

    async fn abort_navigation(&self, cancel: &CancellationToken) {
        let _ = self.stop_navigation(cancel).await;
        let _ = self.close().await;
    }

    fn set_state(&self, state: PageState) {
        *self.inner.state.lock().unwrap() = state;
    }

    fn ensure_open(&self) -> Result<(), BrowserError> {
        match self.state() {
            PageState::Open | PageState::Navigating => Ok(()),
            PageState::Closed => Err(self.closed_error()),
            PageState::Crashed => Err(BrowserError::BrowserCrashed {
                detail: "page session died with the browser".to_string(),
            }),
        }
    }

    fn closed_error(&self) -> BrowserError {
        BrowserError::Cdp {
            detail: format!("page {} is closed", self.inner.target_id),
        }
    }

    /// Map a transport failure onto `BrowserCrashed` when the CDP socket is
    /// gone (the supervisor registry remains the authority on the process).
    fn transport_error(&self, error: BrowserError) -> BrowserError {
        match error {
            BrowserError::Cdp { detail } if self.inner.client.is_closed() => {
                BrowserError::BrowserCrashed { detail }
            }
            other => other,
        }
    }

    fn document_status(&self) -> Option<i64> {
        let tracker = self.inner.tracker.lock().unwrap();
        let mut best: Option<i64> = None;
        for record in tracker.requests() {
            if record.resource_type == crate::interception::ResourceType::Document {
                if let Some(status) = record.status {
                    best = Some(status);
                }
            }
        }
        best
    }
}

/// Classify a probe JSON object. Priority: rate limit > captcha > slider >
/// access denied > login form > generic interstitial.
pub fn classify_probe(probe: &Value) -> Option<VerificationSignal> {
    let flag = |name: &str| probe.get(name).and_then(Value::as_bool).unwrap_or(false);
    if flag("rate_limited") {
        return Some(VerificationSignal::RateLimit);
    }
    if flag("captcha") {
        return Some(VerificationSignal::Captcha);
    }
    if flag("security_slider") {
        return Some(VerificationSignal::SecuritySlider);
    }
    if flag("access_denied") {
        return Some(VerificationSignal::AccessDenied);
    }
    if flag("login_form") {
        return Some(VerificationSignal::LoginForm);
    }
    if flag("interstitial") {
        return Some(VerificationSignal::Interstitial);
    }
    None
}

/// Per-page event pump: routes network, interception, lifecycle and download
/// events for one CDP session. The pump holds a `Weak` page, so a closed
/// page's pump exits instead of leaking.
async fn pump_events(page: Weak<PageInner>, mut events: broadcast::Receiver<CdpEvent>) {
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "page event pump lagged; events dropped");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let Some(inner) = page.upgrade() else {
            break;
        };
        if inner.closed.load(Ordering::SeqCst) {
            break;
        }
        if event.session_id.as_deref() != Some(inner.session_id.as_str()) {
            continue;
        }
        handle_page_event(&inner, event).await;
    }
}

async fn handle_page_event(inner: &Arc<PageInner>, event: CdpEvent) {
    match event.method.as_str() {
        "Network.requestWillBeSent"
        | "Network.responseReceived"
        | "Network.loadingFinished"
        | "Network.loadingFailed" => {
            let notice = inner.tracker.lock().unwrap().apply(&event);
            if let Some(NetworkNotice::Failed {
                url, error_text, ..
            }) = notice
            {
                tracing::debug!(url = %crate::capture::redact_url(&url), error = %error_text, "page network failure");
            }
        }
        "Fetch.requestPaused" => {
            let decision = inner
                .interceptor
                .handle_request_paused(&event.params, deadline_in(5_000), &CancellationToken::new())
                .await;
            if let Err(error) = decision {
                tracing::warn!(error = %error, "interception handling failed");
            }
        }
        "Page.frameNavigated" => {
            let is_main = event
                .params
                .get("frame")
                .and_then(|frame| frame.get("parentId"))
                .map(Value::is_null)
                .unwrap_or(true);
            if is_main {
                let _ = inner.lifecycle.send(Lifecycle::Navigating);
            }
        }
        "Page.domContentEventFired" => {
            let _ = inner.lifecycle.send(Lifecycle::DomContentLoaded);
        }
        "Page.loadEventFired" => {
            let _ = inner.lifecycle.send(Lifecycle::Loaded);
        }
        "Browser.downloadWillBegin" => {
            let decision = inner.downloads.decide(&event.params);
            if let crate::download::DownloadDecision::Denied { url } = decision {
                *inner.last_download_error.lock().unwrap() =
                    Some(BrowserError::DownloadBlocked { url: url.clone() });
                tracing::info!(url = %crate::capture::redact_url(&url), "download denied by policy");
                let guid = event
                    .params
                    .get("guid")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !guid.is_empty() {
                    let _ = inner
                        .client
                        .send(
                            None,
                            "Browser.cancelDownload",
                            json!({ "guid": guid }),
                            deadline_in(3_000),
                            &CancellationToken::new(),
                        )
                        .await;
                }
            }
        }
        "Browser.downloadProgress" => {
            if let Some(guid) = inner.downloads.over_bound_guid(&event.params) {
                *inner.last_download_error.lock().unwrap() = Some(BrowserError::DownloadBlocked {
                    url: format!("download {guid} exceeded the configured byte bound"),
                });
                let _ = inner
                    .client
                    .send(
                        None,
                        "Browser.cancelDownload",
                        json!({ "guid": guid }),
                        deadline_in(3_000),
                        &CancellationToken::new(),
                    )
                    .await;
            }
        }
        "Runtime.exceptionThrown" => {
            let text = event
                .params
                .get("exceptionDetails")
                .and_then(|details| details.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("uncaught exception");
            tracing::debug!(text, "page javascript exception");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_classification_priority_is_deterministic() {
        assert_eq!(
            classify_probe(&json!({"rate_limited": true, "captcha": true})),
            Some(VerificationSignal::RateLimit)
        );
        assert_eq!(
            classify_probe(&json!({"captcha": true, "login_form": true})),
            Some(VerificationSignal::Captcha)
        );
        assert_eq!(
            classify_probe(&json!({"login_form": true})),
            Some(VerificationSignal::LoginForm)
        );
        assert_eq!(classify_probe(&json!({})), None);
        assert_eq!(classify_probe(&Value::Null), None);
    }

    #[test]
    fn verification_signals_map_to_typed_errors() {
        assert_eq!(
            VerificationSignal::LoginForm.error(),
            BrowserError::AuthenticationRequired
        );
        assert_eq!(
            VerificationSignal::Captcha.error(),
            BrowserError::VerificationRequired {
                kind: VerificationKind::Captcha
            }
        );
        assert_eq!(
            VerificationSignal::RateLimit.error(),
            BrowserError::RateLimited {
                retry_after_ms: None
            }
        );
    }

    #[test]
    fn probe_js_has_generic_landmarks_and_no_site_names() {
        for marker in [
            "faktor-verify-probe",
            "input[type=\"password\"]",
            "captcha",
            "recaptcha",
            "hcaptcha",
            "slider",
            "access denied",
            "rate limit",
        ] {
            assert!(
                VERIFICATION_PROBE_JS.contains(marker),
                "probe must detect {marker}"
            );
        }
        // Built from fragments so this test itself never contains a
        // marketplace name (the crate-level static scan asserts that).
        let sites = [
            format!("{}{}", "168", "8"),
            format!("{}{}", "ali", "baba"),
            format!("{}{}", "mou", "ser"),
            format!("{}{}", "digi", "key"),
            format!("{}{}", "lc", "sc"),
        ];
        for site in sites {
            assert!(
                !VERIFICATION_PROBE_JS.to_ascii_lowercase().contains(&site),
                "probe must carry no site knowledge ({site})"
            );
        }
    }
}
