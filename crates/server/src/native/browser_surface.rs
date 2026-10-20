//! Revision 8 browser surface over the real `faktor-browser` authority.
//!
//! The control contract forbids a mock: when `faktor.browser.enabled` is on
//! and a Chromium executable resolves, the daemon advertises exactly one
//! local adapter and launches real supervised pages. When either condition is
//! absent the surface is typed unavailable; it never reports a connected
//! session, observation or screenshot it did not observe.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use faktor_browser::egress::{DestinationPolicy, HostPattern};
use faktor_browser::launch::resolve_executable;
use faktor_browser::manager::{BrowserConfig, BrowserIdentity, BrowserManager, PagePurpose};
use faktor_browser::page::Page;
use faktor_core::cancellation::CancellationToken;
use faktor_core::time::{Deadline, SystemClock};
use faktor_foundation::browser::{
    BrowserAdapterDraft, BrowserMode, BrowserSession, BrowserSessionState,
};
use faktor_foundation::Foundation;

use super::authed;
use super::foundation_ext::foundation;
use crate::api::AppState;

const ADAPTER_ID: &str = "chromium-local";
const SOURCE: &str = "edition8";
const NAVIGATION_TIMEOUT_MS: u64 = 30_000;
const MAX_OBSERVED_REQUESTS: usize = 500;
const MAX_OBSERVED_CONSOLE: usize = 500;

struct LivePage {
    page: Page,
    seen: HashSet<String>,
}

struct SurfaceState {
    manager: Option<Arc<BrowserManager>>,
    reason: String,
}

/// One browser authority per daemon dependency envelope. Holding it here (not
/// in a process-global map) is deliberate: when the daemon drops its deps the
/// `BrowserManager` drop kills every supervised browser tree, so a daemon exit
/// cannot leave orphaned Chromium children behind.
pub(crate) struct BrowserSurface {
    root: PathBuf,
    supervisor: Option<Arc<faktor_terminal::ProcessSupervisor>>,
    state: OnceLock<SurfaceState>,
    pages: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<LivePage>>>>,
}

impl BrowserSurface {
    pub(crate) fn new(
        root: PathBuf,
        supervisor: Option<Arc<faktor_terminal::ProcessSupervisor>>,
    ) -> Self {
        Self {
            root,
            supervisor,
            state: OnceLock::new(),
            pages: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Test/embedded envelope without a supervisor: every browser operation
    /// is a typed unavailable state and no process is ever spawned.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        Self::new(PathBuf::new(), None)
    }

    fn enabled(foundation: &Foundation) -> bool {
        foundation
            .config
            .value("faktor.browser.enabled")
            .ok()
            .flatten()
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    /// Initialize the manager on the first enabled read. This is where the
    /// real executable and supervised profile store are resolved; a failure
    /// is preserved as a typed reason instead of being retried silently.
    pub(crate) fn ensure(&self, foundation: &Foundation) -> Result<(), String> {
        if !Self::enabled(foundation) {
            return Err("browser automation is disabled by faktor.browser.enabled".to_string());
        }
        let state = self.state.get_or_init(|| {
            let Some(supervisor) = self.supervisor.clone() else {
                return SurfaceState {
                    manager: None,
                    reason: "the daemon process supervisor is not wired".to_string(),
                };
            };
            let configured = std::env::var_os("FAKTOR_BROWSER_EXECUTABLE").map(PathBuf::from);
            match resolve_executable(configured.as_deref()) {
                Ok(executable) => {
                    let config = BrowserConfig {
                        enabled: true,
                        executable: Some(executable),
                        headless: true,
                        ..BrowserConfig::default()
                    };
                    match BrowserManager::new(supervisor, config, &self.root.join("browser")) {
                        Ok(manager) => SurfaceState {
                            manager: Some(manager),
                            reason: String::new(),
                        },
                        Err(error) => SurfaceState {
                            manager: None,
                            reason: format!("browser manager: {error}"),
                        },
                    }
                }
                Err(error) => SurfaceState {
                    manager: None,
                    reason: format!("browser executable: {error}"),
                },
            }
        });
        if state.manager.is_some() {
            Ok(())
        } else {
            Err(state.reason.clone())
        }
    }

    pub(crate) fn available(&self) -> bool {
        self.state
            .get()
            .map(|state| state.manager.is_some())
            .unwrap_or(false)
    }

    pub(crate) fn reason(&self) -> Option<String> {
        self.state.get().map(|state| state.reason.clone())
    }

    /// Advertise the real adapter into the durable foundation catalog. The
    /// row is only written when the manager is actually available.
    pub(crate) fn advertise(&self, foundation: &Foundation) -> Result<(), String> {
        self.ensure(foundation)?;
        foundation
            .browser
            .upsert_adapter(&BrowserAdapterDraft {
                id: ADAPTER_ID.into(),
                mode: BrowserMode::Local,
                endpoint: None,
                account_scope: Some("local-machine".into()),
                advertised: true,
                network_scope: "egress_policy".into(),
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn open(
        &self,
        foundation: &Foundation,
        session_id: u64,
        url: &str,
    ) -> Result<BrowserSession, String> {
        self.ensure(foundation)?;
        let manager = self
            .state
            .get()
            .and_then(|state| state.manager.as_ref())
            .ok_or_else(|| "browser adapter is unavailable".to_string())?;
        self.advertise(foundation)?;
        let parsed =
            url::Url::parse(url).map_err(|error| format!("url does not parse: {error}"))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| "url has no host".to_string())?
            .to_string();
        let session = foundation
            .browser
            .request_session(ADAPTER_ID, session_id, url)
            .map_err(|error| error.to_string())?;
        let port = parsed.port_or_known_default().unwrap_or(80);
        let allow_loopback = foundation
            .config
            .value("faktor.sandbox.allowLoopback")
            .ok()
            .flatten()
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let policy = DestinationPolicy::first_party_only(vec![
            HostPattern::parse(&host).map_err(|error| error.to_string())?
        ])
        .with_allow_loopback(allow_loopback)
        .with_allowed_ports(vec![port]);
        let identity = BrowserIdentity::new("local-machine", "edition8", "direct");
        let purpose = PagePurpose::incognito("e8-preview");
        let cancel = CancellationToken::new();
        let deadline = Deadline::now_plus(&SystemClock, NAVIGATION_TIMEOUT_MS);
        let page = match manager
            .acquire_page(SOURCE, &identity, policy, &purpose, deadline, &cancel)
            .await
        {
            Ok(page) => page,
            Err(error) => {
                let _ = foundation
                    .browser
                    .record_state(&session.id, BrowserSessionState::Failed);
                return Err(format!("browser page acquisition: {error}"));
            }
        };
        let navigation = match page.navigate(url, deadline, &cancel).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let code = error.code();
                let _ = foundation
                    .browser
                    .record_state(&session.id, BrowserSessionState::Failed);
                if matches!(code, "download_blocked" | "download_rejected") {
                    let _ = foundation.browser.append_observation(
                        &session.id,
                        "download",
                        Some("error"),
                        &serde_json::json!({
                            "url": url,
                            "policy": "default_deny",
                            "blocked": true,
                            "error": code,
                            "detail": error.to_string(),
                            "filename": title_or_url_filename(url),
                        }),
                    );
                }
                return Err(format!("browser navigation: {error}"));
            }
        };
        let _ = foundation.browser.append_observation(
            &session.id,
            "navigation",
            Some(
                if navigation.lifecycle == faktor_browser::page::Lifecycle::Failed {
                    "error"
                } else {
                    "info"
                },
            ),
            &serde_json::json!({
                "url": navigation.url,
                "status": navigation.status,
                "lifecycle": format!("{:?}", navigation.lifecycle).to_ascii_lowercase(),
                "error": navigation.error_text,
            }),
        );
        let mut seen = HashSet::new();
        self.sync_observations_locked(foundation, &session.id, &page, &mut seen)
            .await;
        self.pages.lock().await.insert(
            session.id.clone(),
            Arc::new(tokio::sync::Mutex::new(LivePage { page, seen })),
        );
        let _ = foundation
            .browser
            .record_state(&session.id, BrowserSessionState::Connected);
        Ok(session)
    }

    async fn sync_observations_locked(
        &self,
        foundation: &Foundation,
        browser_session_id: &str,
        page: &Page,
        seen: &mut HashSet<String>,
    ) {
        let Ok(requests) = page.network() else {
            return;
        };
        for request in requests.into_iter().take(MAX_OBSERVED_REQUESTS) {
            if !seen.insert(format!("network:{}", request.request_id)) {
                continue;
            }
            let severity = if request.failed.is_some() || request.blocked_reason.is_some() {
                "error"
            } else {
                "info"
            };
            let _ = foundation.browser.append_observation(
                browser_session_id,
                "network",
                Some(severity),
                &serde_json::json!({
                    "requestId": request.request_id,
                    "url": request.url,
                    "method": request.method,
                    "resourceType": format!("{:?}", request.resource_type).to_ascii_lowercase(),
                    "status": request.status,
                    "mimeType": request.mime_type,
                    "fromCache": request.from_cache,
                    "encodedDataLength": request.encoded_data_length,
                    "failed": request.failed,
                    "blockedReason": request.blocked_reason,
                    "truncated": request.truncated,
                }),
            );
        }
        let Ok(console) = page.console() else {
            return;
        };
        for entry in console.into_iter().take(MAX_OBSERVED_CONSOLE) {
            if !seen.insert(format!("console:{}", entry.sequence)) {
                continue;
            }
            let severity = match entry.level.as_str() {
                "error" | "assert" => "error",
                "warning" => "warning",
                _ => "info",
            };
            let _ = foundation.browser.append_observation(
                browser_session_id,
                "console",
                Some(severity),
                &serde_json::json!({
                    "sequence": entry.sequence,
                    "level": entry.level,
                    "text": entry.text,
                    "url": entry.url,
                    "line": entry.line,
                    "column": entry.column,
                    "timestampMs": entry.timestamp_ms,
                    "truncated": entry.truncated,
                }),
            );
        }
    }

    pub(crate) async fn sync_observations(
        &self,
        foundation: &Foundation,
        browser_session_id: &str,
    ) -> Result<(), String> {
        let live = {
            let pages = self.pages.lock().await;
            pages.get(browser_session_id).cloned()
        };
        let Some(live) = live else {
            return Ok(());
        };
        let mut guard = live.lock().await;
        let page = guard.page.clone();
        let mut seen = std::mem::take(&mut guard.seen);
        self.sync_observations_locked(foundation, browser_session_id, &page, &mut seen)
            .await;
        guard.seen = seen;
        Ok(())
    }

    pub(crate) async fn screenshot(
        &self,
        browser_session_id: &str,
        format: &str,
    ) -> Result<Vec<u8>, String> {
        let live = {
            let pages = self.pages.lock().await;
            pages.get(browser_session_id).cloned()
        };
        let Some(live) = live else {
            return Err("no live page is attached to this browser session".into());
        };
        let page = live.lock().await.page.clone();
        let cancel = CancellationToken::new();
        let deadline = Deadline::now_plus(&SystemClock, NAVIGATION_TIMEOUT_MS);
        page.screenshot(format, deadline, &cancel)
            .await
            .map(|captured| captured.bytes)
            .map_err(|error| format!("browser screenshot: {error}"))
    }
}

fn title_or_url_filename(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "download".to_string())
}

pub(crate) fn adapter_id() -> &'static str {
    ADAPTER_ID
}

/// `GET /native/browser/status` — the honest attachment state of the one
/// browser authority. It never says "available" for a disabled surface and
/// never reports an adapter it did not advertise.
pub(crate) async fn native_browser_status(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if let Err(e) = authed(&headers, &state) {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(e.to_json()),
        )
            .into_response();
    }
    let foundation = match foundation(&state) {
        Ok(foundation) => foundation,
        Err(response) => return response,
    };
    let enabled = foundation
        .config
        .value("faktor.browser.enabled")
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let surface = &state.deps.browser;
    let available = surface.ensure(foundation).is_ok();
    axum::Json(serde_json::json!({
        "enabled": enabled,
        "available": available,
        "attached": available,
        "adapters": [{
            "id": ADAPTER_ID,
            "families": ["chromium"],
            "state": if available { "advertised" } else if enabled { "unavailable" } else { "disabled" },
            "detail": if available {
                "a real local Chromium adapter is attached and launches supervised pages".to_string()
            } else {
                surface.reason().unwrap_or_else(|| "no browser authority is attached".to_string())
            },
        }],
        "reason": if available { serde_json::Value::Null } else {
            serde_json::json!(surface.reason().unwrap_or_else(|| "no browser authority is attached".to_string()))
        },
    }))
    .into_response()
}
