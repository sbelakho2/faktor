//! The browser manager: lazy supervised startup, profile/page bounds, stable
//! identities, idle shutdown, crash detection and daemon teardown.
//!
//! One [`BrowserManager`] owns every browser child of the process. A browser
//! is started lazily on the first page acquisition for an identity and is
//! killed by [`BrowserManager::shutdown_idle`] once it has been unused for
//! `idle_shutdown_s` (the daemon's maintenance tick calls this; there is no
//! hidden background loop). Bounds (`max_browsers`,
//! `max_pages_per_profile`) refuse with typed errors before a process
//! exists. A crashed child is detected from the supervisor registry, its
//! profile stops accepting work, and no zombie is left behind.
//!
//! # Instance identity and admission
//!
//! Every map entry is keyed by [`BrowserInstanceId`] (identity + mode), so a
//! persistent profile and an incognito context with the same account/profile
//! /egress label are two distinct browsers that can never shadow each other.
//!
//! Each instance owns a small lifecycle state machine (`Live` -> `Retiring`
//! -> `Dead`, under its own async mutex) plus a semaphore of
//! `max_pages_per_profile` permits. `Page` owns one permit, so a dropped or
//! abandoned page releases its slot synchronously. Page admission and idle
//! retirement meet on the lifecycle lock: retirement only flips to
//! `Retiring` when no permit is held (no page in flight) and removes the
//! exact map entry; acquisitions that raced it observe `Retiring` and refuse
//! typed instead of receiving a page whose browser is being closed
//! underneath.
//!
//! # Launch transaction
//!
//! Construction is an explicit transaction: anything created after the
//! broker starts (context, CDP client, supervised child, temporary profile)
//! is rolled back by one `rollback_launch` on every error path, and
//! [`LaunchedBrowser`] additionally kills an armed child on drop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::{Clock, Deadline, SystemClock};
use faktor_terminal::{ProcessOwner, ProcessSupervisor};

use crate::capture::CaptureLimits;
use crate::cdp::{CdpClient, CdpConfig};
use crate::download::{DownloadManager, DownloadPolicy};
use crate::egress::{
    BrokerConfig, BrokerHandle, DestinationPolicy, EgressBroker, UpstreamSelector,
};
use crate::error::BrowserError;
use crate::interception::{InterceptionPolicy, Interceptor};
use crate::launch::{
    resolve_executable, BrowserIsolationState, ChromiumLauncher, LaunchOptions, LaunchedBrowser,
    KILL_GRACE_MS,
};
use crate::page::{Page, PageHost, PageInner, PageState, VerificationSignal};
use crate::profile::{validate_profile_name, IncognitoProfile, ProfileStore, MAX_ACCOUNT_BYTES};
use crate::timeutil::deadline_in;

/// Runtime configuration (mirrors spec §13 `commerce.browser`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserConfig {
    pub enabled: bool,
    pub executable: Option<PathBuf>,
    pub headless: bool,
    /// Kill an unused browser after this many seconds (default 300).
    pub idle_shutdown_s: u64,
    /// Maximum simultaneously live browser children (default 2).
    pub max_browsers: usize,
    /// Maximum simultaneously open pages per profile (default 1).
    pub max_pages_per_profile: usize,
    pub launch_timeout_ms: u64,
    pub navigation_timeout_ms: u64,
    /// Bounded observation fan-out capacity (lossy; a lag only latches a
    /// history gap on the page).
    pub cdp_event_capacity: usize,
    /// Bounded per-session critical CDP event queue (paused requests,
    /// lifecycle, termination). An overflow fails the page loudly.
    pub cdp_critical_event_capacity: usize,
    /// Operational extra Chromium flags (validated: egress/control-plane
    /// overrides are refused).
    pub extra_args: Vec<String>,
    pub capture: CaptureLimits,
    pub downloads: DownloadPolicy,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: None,
            headless: true,
            idle_shutdown_s: 300,
            max_browsers: 2,
            max_pages_per_profile: 1,
            launch_timeout_ms: 20_000,
            navigation_timeout_ms: 30_000,
            cdp_event_capacity: 2048,
            cdp_critical_event_capacity: 256,
            extra_args: Vec::new(),
            capture: CaptureLimits::default(),
            downloads: DownloadPolicy::default(),
        }
    }
}

impl BrowserConfig {
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.idle_shutdown_s == 0 || self.idle_shutdown_s > 86_400 {
            return Err(BrowserError::invalid_config(
                "idle_shutdown_s must be 1..=86400",
            ));
        }
        if self.max_browsers == 0 || self.max_browsers > 16 {
            return Err(BrowserError::invalid_config("max_browsers must be 1..=16"));
        }
        if self.max_pages_per_profile == 0 || self.max_pages_per_profile > 8 {
            return Err(BrowserError::invalid_config(
                "max_pages_per_profile must be 1..=8",
            ));
        }
        if self.launch_timeout_ms < 100 || self.launch_timeout_ms > 600_000 {
            return Err(BrowserError::invalid_config(
                "launch_timeout_ms must be 100..=600000",
            ));
        }
        if self.navigation_timeout_ms < 100 || self.navigation_timeout_ms > 600_000 {
            return Err(BrowserError::invalid_config(
                "navigation_timeout_ms must be 100..=600000",
            ));
        }
        if self.cdp_event_capacity == 0
            || self.cdp_event_capacity > crate::cdp::CDP_MAX_CAPACITY_CEILING
        {
            return Err(BrowserError::invalid_config(format!(
                "cdp_event_capacity must be 1..={}",
                crate::cdp::CDP_MAX_CAPACITY_CEILING
            )));
        }
        if self.cdp_critical_event_capacity == 0
            || self.cdp_critical_event_capacity > crate::cdp::CDP_MAX_CAPACITY_CEILING
        {
            return Err(BrowserError::invalid_config(format!(
                "cdp_critical_event_capacity must be 1..={}",
                crate::cdp::CDP_MAX_CAPACITY_CEILING
            )));
        }
        self.capture.validate()?;
        CdpConfig {
            max_message_bytes: self.capture.max_cdp_message_bytes,
            event_capacity: self.cdp_event_capacity,
            critical_event_capacity: self.cdp_critical_event_capacity,
        }
        .validate()?;
        self.downloads.validate()?;
        crate::launch::validate_extra_args(&self.extra_args)?;
        Ok(())
    }
}

/// The stable browser identity (spec §9): account, profile and egress route
/// name. Egress changes only for operational reasons (retire + relaunch).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserIdentity {
    pub account: String,
    pub profile: String,
    pub egress: String,
}

impl BrowserIdentity {
    pub fn new(
        account: impl Into<String>,
        profile: impl Into<String>,
        egress: impl Into<String>,
    ) -> Self {
        Self {
            account: account.into(),
            profile: profile.into(),
            egress: egress.into(),
        }
    }

    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.account.is_empty()
            || self.account.len() > MAX_ACCOUNT_BYTES
            || self.account.chars().any(char::is_control)
        {
            return Err(BrowserError::invalid_config(
                "browser account label must be 1..=128 bytes with no control characters",
            ));
        }
        validate_profile_name(&self.profile)?;
        validate_egress_name(&self.egress)?;
        Ok(())
    }

    fn key(&self) -> String {
        format!("{}\u{1}{}\u{1}{}", self.account, self.profile, self.egress)
    }
}

/// Whether an instance serves the persistent profile directory or a
/// temporary incognito context. The mode is part of every instance key, so
/// the two can never be conflated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrowserMode {
    Persistent,
    Incognito,
}

impl BrowserMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BrowserMode::Persistent => "persistent",
            BrowserMode::Incognito => "incognito",
        }
    }
}

/// The full address of one live browser instance: the stable identity plus
/// the isolation mode. Every manager map operation is keyed by this type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserInstanceId {
    pub identity: BrowserIdentity,
    pub mode: BrowserMode,
}

impl BrowserInstanceId {
    pub fn persistent(identity: BrowserIdentity) -> Self {
        Self {
            identity,
            mode: BrowserMode::Persistent,
        }
    }

    pub fn incognito(identity: BrowserIdentity) -> Self {
        Self {
            identity,
            mode: BrowserMode::Incognito,
        }
    }

    pub fn mode(&self) -> BrowserMode {
        self.mode
    }

    /// The stable textual key used for diagnostics and drop-time owner
    /// scoping (`\u{1}` separators keep components unambiguous).
    pub fn key(&self) -> String {
        format!("{}\u{1}{}", self.identity.key(), self.mode.as_str())
    }
}

/// The connector source label used for `ProcessOwner::Browser { source,
/// profile }` kill scoping. Sources are connector identifiers supplied by
/// the caller (e.g. `example-source`) — bounded, no paths, never interpreted
/// here.
pub fn validate_source_name(name: &str) -> Result<(), BrowserError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(BrowserError::invalid_config(format!(
            "browser source name {name:?} must match [A-Za-z0-9._-]{{1,64}}"
        )));
    }
    Ok(())
}

/// Egress route names share the profile-name grammar.
pub fn validate_egress_name(name: &str) -> Result<(), BrowserError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(BrowserError::invalid_config(format!(
            "egress route name {name:?} must match [a-z0-9_-]{{1,64}}"
        )));
    }
    Ok(())
}

/// What a page is for. The name is a bounded operational label (e.g.
/// "extraction") and is never interpreted as site knowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagePurpose {
    pub name: String,
    /// Use a temporary incognito context instead of the persistent profile.
    pub incognito: bool,
}

impl PagePurpose {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            incognito: false,
        }
    }

    pub fn incognito(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            incognito: true,
        }
    }

    fn validate(&self) -> Result<(), BrowserError> {
        if self.name.is_empty() || self.name.len() > 64 || self.name.chars().any(char::is_control) {
            return Err(BrowserError::invalid_config(
                "page purpose must be 1..=64 bytes with no control characters",
            ));
        }
        Ok(())
    }
}

/// Health of one live browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserHealth {
    pub account: String,
    pub profile: String,
    pub egress: String,
    pub mode: BrowserMode,
    pub pid: u32,
    pub child_id: u64,
    pub state: BrowserState,
    pub pages: usize,
    pub proxy_url: String,
    /// The ACTUAL network-isolation strength of this child (never an
    /// aspiration): OS-confined BrokerOnly where the sandbox namespace was
    /// produced, app-level proxy-only everywhere else. [`BrowserIsolationState::strength_label`]
    /// is the honest one-line spelling every surface must print.
    pub isolation: BrowserIsolationState,
    pub last_used_ms: i64,
    pub requests_total: u64,
    pub blocked_total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserState {
    /// Live with at least one open page.
    Active,
    /// Live with no open pages (an idle-shutdown candidate).
    Idle,
    /// Live but stopped: human verification is required for this profile.
    Stopped,
    /// The child is gone.
    Crashed,
}

/// Per-instance lifecycle under one async mutex. `Retiring` is the point of
/// no return: no new permit may be taken, and an already-admitted page (a
/// held permit) keeps the instance from ever entering `Retiring` through the
/// idle path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstanceLifecycle {
    Live,
    Retiring,
    Dead,
}

/// A deterministic test seam for the admission/retirement decision points.
/// Production code only ever checks for its presence; integration tests
/// install one to drive the exact interleavings by hand.
#[doc(hidden)]
#[derive(Clone)]
pub struct LifecycleSeam {
    profile: String,
    reached: tokio::sync::mpsc::UnboundedSender<&'static str>,
    release: Arc<tokio::sync::Semaphore>,
}

impl LifecycleSeam {
    /// Build a seam for one profile plus the receiver that reports each
    /// reached pause point.
    pub fn new(
        profile: impl Into<String>,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<&'static str>) {
        let (reached, receiver) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                profile: profile.into(),
                reached,
                release: Arc::new(tokio::sync::Semaphore::new(0)),
            },
            receiver,
        )
    }

    /// Release `count` paused pause points.
    pub fn grant(&self, count: usize) {
        self.release.add_permits(count);
    }

    async fn pause(&self, profile: &str, point: &'static str) {
        if profile != self.profile {
            return;
        }
        let _ = self.reached.send(point);
        let _ = self.release.acquire().await;
    }
}

/// A live browser instance.
pub struct BrowserInstance {
    id: BrowserInstanceId,
    source: String,
    launched: LaunchedBrowser,
    client: CdpClient,
    broker: BrokerHandle,
    #[allow(dead_code)] // retained for diagnostics; the profile store is the accessor
    profile_dir: PathBuf,
    #[allow(dead_code)]
    incognito: Option<IncognitoProfile>,
    context_id: Option<String>,
    policy: DestinationPolicy,
    /// Live pages as weak handles: an abandoned/cancelled capture whose last
    /// `Page` handle drops releases its profile slot automatically (the
    /// permit lives in `PageInner`; see `Page::attach`).
    pages: Mutex<HashMap<String, Weak<PageInner>>>,
    stop: Mutex<Option<VerificationSignal>>,
    crashed: AtomicBool,
    last_used_ms: AtomicI64,
    clock: Arc<dyn Clock>,
    /// The per-instance lifecycle state machine.
    lifecycle: tokio::sync::Mutex<InstanceLifecycle>,
    /// The page-admission permits (one `Page` owns one permit).
    admission: Arc<tokio::sync::Semaphore>,
    /// The root-session event pump (browser-domain download events).
    event_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Central download authority for this profile.
    downloads: Arc<DownloadManager>,
}

impl BrowserInstance {
    fn identity(&self) -> &BrowserIdentity {
        &self.id.identity
    }

    fn touch(&self) {
        self.last_used_ms
            .store(self.clock.now_ms(), Ordering::SeqCst);
    }

    fn stop_signal(&self) -> Option<VerificationSignal> {
        *self.stop.lock().unwrap()
    }

    fn pages_snapshot(&self) -> Vec<Page> {
        self.pages
            .lock()
            .unwrap()
            .values()
            .filter_map(Page::upgrade)
            .collect()
    }

    fn page_count(&self) -> usize {
        self.pages
            .lock()
            .unwrap()
            .values()
            .filter(|weak| weak.strong_count() > 0)
            .count()
    }

    /// Is the supervised child still in the live registry?
    fn child_is_alive(&self, supervisor: &ProcessSupervisor) -> bool {
        supervisor
            .alive()
            .iter()
            .any(|handle| handle.id == self.launched.child_id)
    }

    /// Verify liveness: a dead child (or a dead CDP socket) is a typed
    /// crash; the instance is marked and its whole tree is killed so no
    /// orphan survives.
    fn check_alive(&self, supervisor: &ProcessSupervisor) -> Result<(), BrowserError> {
        if self.crashed.load(Ordering::SeqCst) {
            return Err(BrowserError::BrowserCrashed {
                detail: format!(
                    "browser for profile {} crashed earlier",
                    self.identity().profile
                ),
            });
        }
        if !self.child_is_alive(supervisor) || self.client.is_closed() {
            self.crashed.store(true, Ordering::SeqCst);
            let _ = supervisor.kill(self.launched.child_id, KILL_GRACE_MS);
            let reason = self
                .client
                .closed_reason()
                .unwrap_or_else(|| "supervised child exited".to_string());
            for page in self.pages_snapshot() {
                page.mark_crashed();
            }
            return Err(BrowserError::BrowserCrashed {
                detail: format!(
                    "browser pid {} for profile {} is gone: {reason}",
                    self.launched.pid,
                    self.identity().profile
                ),
            });
        }
        Ok(())
    }
}

impl PageHost for BrowserInstance {
    fn release_page(&self, target_id: &str) {
        self.pages.lock().unwrap().remove(target_id);
    }

    fn stop_automation(&self, signal: VerificationSignal) {
        let mut stop = self.stop.lock().unwrap();
        if stop.is_none() {
            *stop = Some(signal);
        }
        drop(stop);
        tracing::info!(
            profile = %self.identity().profile,
            signal = signal.kind().as_str(),
            "browser profile stopped: human verification required"
        );
        // Release the profile's page slots immediately (bounded work), then
        // close the targets in the background.
        let pages: Vec<Page> = {
            let mut map = self.pages.lock().unwrap();
            map.drain()
                .filter_map(|(_, weak)| Page::upgrade(&weak))
                .collect()
        };
        for page in pages {
            // Release the slot synchronously (the old drain semantics): a
            // stopped profile refuses new work, and the slot must never stay
            // occupied by a page the human is about to re-do by hand.
            page.release_permit();
            tokio::spawn(async move {
                let _ = page.close().await;
            });
        }
    }
}

/// The manager. Cheap to clone via `Arc`.
pub struct BrowserManager {
    supervisor: Arc<ProcessSupervisor>,
    config: BrowserConfig,
    profiles: ProfileStore,
    launcher: ChromiumLauncher,
    browsers: Mutex<HashMap<BrowserInstanceId, Arc<BrowserInstance>>>,
    egress_routes: Mutex<HashMap<String, UpstreamSelector>>,
    create_serial: tokio::sync::Mutex<()>,
    clock: Arc<dyn Clock>,
    lifecycle_seam: Mutex<Option<LifecycleSeam>>,
}

impl std::fmt::Debug for BrowserManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserManager")
            .field("browsers", &self.browsers.lock().unwrap().len())
            .field("config", &self.config)
            .finish()
    }
}

impl BrowserManager {
    /// Create a manager rooted at `<data_dir>/commerce/profiles`.
    pub fn new(
        supervisor: Arc<ProcessSupervisor>,
        config: BrowserConfig,
        data_dir: &std::path::Path,
    ) -> Result<Arc<Self>, BrowserError> {
        Self::with_clock(supervisor, config, data_dir, Arc::new(SystemClock))
    }

    pub fn with_clock(
        supervisor: Arc<ProcessSupervisor>,
        config: BrowserConfig,
        data_dir: &std::path::Path,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, BrowserError> {
        config.validate()?;
        let profiles = ProfileStore::open(data_dir.join("commerce").join("profiles"))?;
        Ok(Arc::new(Self {
            launcher: ChromiumLauncher::new(supervisor.clone()),
            supervisor,
            config,
            profiles,
            browsers: Mutex::new(HashMap::new()),
            egress_routes: Mutex::new(HashMap::new()),
            create_serial: tokio::sync::Mutex::new(()),
            clock,
            lifecycle_seam: Mutex::new(None),
        }))
    }

    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    pub fn profiles(&self) -> &ProfileStore {
        &self.profiles
    }

    /// Install (or clear) the deterministic lifecycle seam.
    #[doc(hidden)]
    pub fn set_lifecycle_seam(&self, seam: Option<LifecycleSeam>) {
        *self.lifecycle_seam.lock().unwrap() = seam;
    }

    async fn pause_seam(&self, profile: &str, point: &'static str) {
        let seam = self.lifecycle_seam.lock().unwrap().clone();
        if let Some(seam) = seam {
            seam.pause(profile, point).await;
        }
    }

    /// Register an upstream egress route (operational configuration, never
    /// model input). Unknown route names refuse acquisition typed.
    pub fn register_egress(
        &self,
        name: &str,
        selector: UpstreamSelector,
    ) -> Result<(), BrowserError> {
        validate_egress_name(name)?;
        self.egress_routes
            .lock()
            .unwrap()
            .insert(name.to_string(), selector);
        Ok(())
    }

    /// Acquire a page for an identity, starting the browser lazily. `source`
    /// is the connector source used for the supervisor owner scope.
    pub async fn acquire_page(
        &self,
        source: &str,
        identity: &BrowserIdentity,
        policy: DestinationPolicy,
        purpose: &PagePurpose,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Page, BrowserError> {
        if !self.config.enabled {
            return Err(BrowserError::Disabled);
        }
        validate_source_name(source)?;
        identity.validate()?;
        purpose.validate()?;
        policy.validate()?;
        let instance = self
            .instance_for(source, identity, &policy, purpose, deadline, cancel)
            .await?;
        self.pause_seam(&identity.profile, "admission-begin").await;
        let permit = {
            let state = instance.lifecycle.lock().await;
            if let Some(signal) = instance.stop_signal() {
                return Err(signal.error());
            }
            if *state != InstanceLifecycle::Live {
                return Err(BrowserError::retiring(format!(
                    "browser for profile {} is retiring; retry starts a fresh browser",
                    identity.profile
                )));
            }
            if let Err(error) = instance.check_alive(&self.supervisor) {
                drop(state);
                self.drop_instance(&instance);
                return Err(error);
            }
            match instance.admission.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    let open =
                        self.config.max_pages_per_profile - instance.admission.available_permits();
                    return Err(BrowserError::bound(format!(
                        "profile {} already has {open} open page(s); max_pages_per_profile is {}",
                        identity.profile, self.config.max_pages_per_profile
                    )));
                }
            }
        };
        instance.touch();
        self.pause_seam(&identity.profile, "admission-permit").await;
        let page = self
            .open_page(&instance, &policy, deadline, cancel, permit)
            .await
            .map_err(|error| {
                // A failed page open on a dead browser must not leave the
                // instance behind.
                if matches!(error, BrowserError::BrowserCrashed { .. }) {
                    self.drop_instance(&instance);
                }
                error
            })?;
        Ok(page)
    }

    /// Open one target and initialize it transactionally: once a target id
    /// exists, every remaining init step runs in one result block, and any
    /// failure closes the target with a fresh cancellation token and a
    /// bounded cleanup deadline. The cleanup is disarmed only when the page
    /// has been inserted into instance ownership.
    async fn open_page(
        &self,
        instance: &Arc<BrowserInstance>,
        policy: &DestinationPolicy,
        deadline: Deadline,
        cancel: &CancellationToken,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<Page, BrowserError> {
        let client = instance.client.clone();
        let mut create_params = serde_json::json!({ "url": "about:blank" });
        if let Some(context_id) = &instance.context_id {
            create_params["browserContextId"] = serde_json::json!(context_id);
        }
        let created = client
            .send(None, "Target.createTarget", create_params, deadline, cancel)
            .await?;
        // Before this point there is no target to clean up; from here on the
        // transaction owns it.
        let target_id = created
            .get("targetId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BrowserError::cdp("Target.createTarget returned no targetId"))?
            .to_string();
        let initialized = async {
            let attached = client
                .send(
                    None,
                    "Target.attachToTarget",
                    serde_json::json!({ "targetId": target_id, "flatten": true }),
                    deadline,
                    cancel,
                )
                .await?;
            let session_id = attached
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| BrowserError::cdp("Target.attachToTarget returned no sessionId"))?
                .to_string();
            for method in ["Page.enable", "Network.enable", "Runtime.enable"] {
                client
                    .send(
                        Some(&session_id),
                        method,
                        serde_json::json!({}),
                        deadline,
                        cancel,
                    )
                    .await?;
            }
            let interceptor = Interceptor::new(
                client.clone(),
                session_id.clone(),
                InterceptionPolicy::new(policy.clone()),
            );
            let (enable_method, mut enable_params) = interceptor.enable_command();
            enable_params["patterns"] = instance.downloads.fetch_patterns();
            client
                .send(
                    Some(&session_id),
                    enable_method,
                    enable_params,
                    deadline,
                    cancel,
                )
                .await?;
            let host: Weak<dyn PageHost> = Arc::downgrade(&(instance.clone() as Arc<dyn PageHost>));
            Ok::<_, BrowserError>(Page::attach(
                client.clone(),
                target_id.clone(),
                session_id,
                interceptor,
                instance.downloads.clone(),
                self.config.capture.clone(),
                crate::network::NetworkLimits {
                    max_records: self.config.capture.max_network_records,
                    max_body_bytes: self.config.capture.max_body_bytes,
                    hard_max_body_bytes: self.config.capture.hard_max_body_bytes,
                },
                host,
                permit,
            ))
        }
        .await;
        match initialized {
            Ok(page) => {
                instance
                    .pages
                    .lock()
                    .unwrap()
                    .insert(target_id, page.downgrade());
                Ok(page)
            }
            Err(error) => {
                // Fresh cleanup token + bounded deadline: the caller's token
                // may already be cancelled, but the created target must
                // still die.
                let _ = client
                    .send(
                        None,
                        "Target.closeTarget",
                        serde_json::json!({ "targetId": target_id }),
                        deadline_in(2_000),
                        &CancellationToken::new(),
                    )
                    .await;
                Err(error)
            }
        }
    }

    /// Get the live instance for an identity, creating (and launching) it
    /// when missing. Serialized so concurrent acquisitions cannot overshoot
    /// `max_browsers`.
    async fn instance_for(
        &self,
        source: &str,
        identity: &BrowserIdentity,
        policy: &DestinationPolicy,
        purpose: &PagePurpose,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Arc<BrowserInstance>, BrowserError> {
        let _serial = self.create_serial.lock().await;
        let id = BrowserInstanceId {
            identity: identity.clone(),
            mode: if purpose.incognito {
                BrowserMode::Incognito
            } else {
                BrowserMode::Persistent
            },
        };
        if let Some(instance) = self.browsers.lock().unwrap().get(&id).cloned() {
            if instance.policy != *policy {
                return Err(BrowserError::invalid_config(
                    "destination policy changed for a live browser identity; retire the browser \
                     to change egress policy",
                ));
            }
            if instance.source != source {
                return Err(BrowserError::invalid_config(
                    "browser source changed for a live browser identity; retire the browser first",
                ));
            }
            // Belt-and-braces: the id already carries the mode, so a
            // persistent page can never be handed to an incognito request
            // (or vice versa).
            if instance.context_id.is_some() != purpose.incognito {
                return Err(BrowserError::invalid_config(
                    "page purpose isolation changed for a live browser identity; retire the \
                     browser first",
                ));
            }
            return Ok(instance);
        }
        {
            let live = self.browsers.lock().unwrap().len();
            if live >= self.config.max_browsers {
                return Err(BrowserError::bound(format!(
                    "max_browsers ({}) reached; refusing to start another browser",
                    self.config.max_browsers
                )));
            }
        }
        let instance = self
            .launch_instance(source, &id, policy, purpose, deadline, cancel)
            .await?;
        self.browsers.lock().unwrap().insert(id, instance.clone());
        Ok(instance)
    }

    async fn launch_instance(
        &self,
        source: &str,
        id: &BrowserInstanceId,
        policy: &DestinationPolicy,
        purpose: &PagePurpose,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Arc<BrowserInstance>, BrowserError> {
        let identity = &id.identity;
        let upstream = {
            let routes = self.egress_routes.lock().unwrap();
            match routes.get(&identity.egress).cloned() {
                Some(selector) => selector,
                None if identity.egress == "direct" => UpstreamSelector::default(),
                None => {
                    return Err(BrowserError::EgressUnavailable {
                        detail: format!("egress route {:?} is not registered", identity.egress),
                    })
                }
            }
        };
        // From here on the broker exists: every error path must shut it down
        // (and drop any temporary profile already created).
        let broker = EgressBroker::start(BrokerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback literal"),
            policy: policy.clone(),
            upstream,
            ..BrokerConfig::default()
        })
        .await?;
        let executable = match resolve_executable(self.config.executable.as_deref()) {
            Ok(executable) => executable,
            Err(error) => {
                broker.shutdown().await;
                return Err(error);
            }
        };
        let mut profiles = match self.prepare_profiles(id, purpose) {
            Ok(profiles) => profiles,
            Err(error) => {
                broker.shutdown().await;
                return Err(error);
            }
        };
        let downloads = match DownloadManager::new(
            self.config.downloads.clone(),
            self.profiles.rooted().clone(),
            &profiles.profile_rel,
        ) {
            Ok(downloads) => downloads,
            Err(error) => {
                profiles.cleanup();
                broker.shutdown().await;
                return Err(error);
            }
        };
        let scratch_dir = self.profiles.rooted().join(&profiles.scratch_rel);
        // Request OS-level BrokerOnly confinement where the platform has a
        // backend; elsewhere the child inherits and the launch carries the
        // honest app-level reason (never silently worded as confinement).
        // The single selection locus is `select_network_isolation`, shared
        // with the doctor surface.
        let (network_isolation, app_level_reason) =
            crate::launch::select_network_isolation(broker.addr());
        let launch = self
            .launcher
            .launch(
                LaunchOptions {
                    executable,
                    headless: self.config.headless,
                    profile_dir: profiles.profile_dir.clone(),
                    scratch_dir,
                    scratch_root: self.profiles.rooted().clone(),
                    scratch_rel: profiles.scratch_rel.clone(),
                    proxy_addr: broker.addr(),
                    owner_source: source.to_string(),
                    owner_profile: identity.profile.clone(),
                    extra_args: self.config.extra_args.clone(),
                    launch_timeout_ms: self.config.launch_timeout_ms,
                    network_isolation,
                    app_level_reason,
                },
                cancel,
            )
            .await;
        let launched = match launch {
            Ok(launched) => launched,
            Err(error) => {
                profiles.cleanup();
                broker.shutdown().await;
                return Err(error);
            }
        };
        // Post-spawn transaction: one rollback for every failure.
        let connect = CdpClient::connect(
            &launched.devtools_ws_url,
            cancel,
            deadline,
            CdpConfig {
                max_message_bytes: self.config.capture.max_cdp_message_bytes,
                event_capacity: self.config.cdp_event_capacity,
                critical_event_capacity: self.config.cdp_critical_event_capacity,
            },
        )
        .await;
        let client = match connect {
            Ok(client) => client,
            Err(error) => {
                self.rollback_launch(launched, None, None, profiles.take_incognito(), broker)
                    .await;
                return Err(error);
            }
        };
        if let Err(error) = client
            .send(
                None,
                "Browser.getVersion",
                serde_json::json!({}),
                deadline,
                cancel,
            )
            .await
        {
            self.rollback_launch(
                launched,
                Some(client),
                None,
                profiles.take_incognito(),
                broker,
            )
            .await;
            return Err(error);
        }
        let (behavior_method, behavior_params) = downloads.behavior_command();
        if let Err(error) = client
            .send(None, behavior_method, behavior_params, deadline, cancel)
            .await
        {
            self.rollback_launch(
                launched,
                Some(client),
                None,
                profiles.take_incognito(),
                broker,
            )
            .await;
            return Err(error);
        }
        let context_id = if purpose.incognito {
            match client
                .send(
                    None,
                    "Target.createBrowserContext",
                    serde_json::json!({}),
                    deadline,
                    cancel,
                )
                .await
            {
                Ok(created) => {
                    match created
                        .get("browserContextId")
                        .and_then(serde_json::Value::as_str)
                    {
                        Some(context_id) => Some(context_id.to_string()),
                        None => {
                            // The context may exist without an id: it
                            // cannot be disposed, but the child, socket,
                            // broker and temporary profile are still torn
                            // down.
                            self.rollback_launch(
                                launched,
                                Some(client),
                                None,
                                profiles.take_incognito(),
                                broker,
                            )
                            .await;
                            return Err(BrowserError::cdp(
                                "Target.createBrowserContext returned no id",
                            ));
                        }
                    }
                }
                Err(error) => {
                    self.rollback_launch(
                        launched,
                        Some(client),
                        None,
                        profiles.take_incognito(),
                        broker,
                    )
                    .await;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let instance = Arc::new(BrowserInstance {
            id: id.clone(),
            source: source.to_string(),
            launched,
            client,
            broker,
            profile_dir: profiles.profile_dir,
            incognito: profiles.incognito,
            context_id,
            policy: policy.clone(),
            pages: Mutex::new(HashMap::new()),
            stop: Mutex::new(None),
            crashed: AtomicBool::new(false),
            last_used_ms: AtomicI64::new(self.clock.now_ms()),
            clock: self.clock.clone(),
            lifecycle: tokio::sync::Mutex::new(InstanceLifecycle::Live),
            admission: Arc::new(tokio::sync::Semaphore::new(
                self.config.max_pages_per_profile,
            )),
            event_task: Mutex::new(None),
            downloads,
        });
        let task = spawn_root_event_pump(&instance);
        *instance.event_task.lock().unwrap() = Some(task);
        Ok(instance)
    }

    /// One rollback for every post-spawn launch failure: dispose a created
    /// browser context, close the CDP socket, kill the supervised child,
    /// await broker shutdown and remove the temporary (incognito) profile.
    async fn rollback_launch(
        &self,
        launched: LaunchedBrowser,
        client: Option<CdpClient>,
        context_id: Option<String>,
        incognito: Option<IncognitoProfile>,
        broker: BrokerHandle,
    ) {
        if let (Some(client), Some(context_id)) = (&client, &context_id) {
            let _ = client
                .send(
                    None,
                    "Target.disposeBrowserContext",
                    serde_json::json!({ "browserContextId": context_id }),
                    deadline_in(2_000),
                    &CancellationToken::new(),
                )
                .await;
        }
        if let Some(client) = &client {
            let _ = client.close().await;
        }
        let _ = launched.kill(&self.supervisor, KILL_GRACE_MS);
        broker.shutdown().await;
        if let Some(incognito) = incognito {
            incognito.remove();
        }
    }

    /// Create the profile/scratch layout for one instance through the
    /// anchored authority.
    fn prepare_profiles(
        &self,
        id: &BrowserInstanceId,
        purpose: &PagePurpose,
    ) -> Result<PreparedProfiles, BrowserError> {
        let identity = &id.identity;
        if purpose.incognito {
            let incognito = self.profiles.incognito(&identity.profile)?;
            let profile_rel = incognito.rel().to_path_buf();
            let scratch_rel = profile_rel.join("scratch");
            self.profiles
                .rooted()
                .create_dir_all(&scratch_rel)
                .map_err(|e| {
                    BrowserError::profile(format!("cannot create incognito scratch: {e}"))
                })?;
            self.profiles
                .rooted()
                .restrict_owner_only(&scratch_rel)
                .map_err(|e| {
                    BrowserError::profile(format!("cannot restrict incognito scratch: {e}"))
                })?;
            Ok(PreparedProfiles {
                profile_dir: incognito.dir(),
                profile_rel,
                scratch_rel,
                incognito: Some(incognito),
            })
        } else {
            let profile_dir = self.profiles.profile_dir(&identity.profile)?;
            let rel = PathBuf::from(&identity.profile);
            let scratch_rel = rel.join("scratch");
            self.profiles.scratch_dir(&identity.profile)?;
            Ok(PreparedProfiles {
                profile_dir,
                profile_rel: rel,
                scratch_rel,
                incognito: None,
            })
        }
    }

    /// Shut down every browser unused for at least `idle_shutdown_s` **and
    /// with no page (and no in-flight admission)**: retirement and page
    /// admission meet on the instance lifecycle lock, so a browser can never
    /// be closed underneath a page that was just admitted. Returns the
    /// profiles that were stopped. The whole tree is killed through the
    /// supervisor, so no orphan survives.
    pub async fn shutdown_idle(&self) -> Vec<String> {
        let now = self.clock.now_ms();
        let idle_ms = (self.config.idle_shutdown_s as i64).saturating_mul(1000);
        let candidates: Vec<Arc<BrowserInstance>> =
            self.browsers.lock().unwrap().values().cloned().collect();
        let mut stopped = Vec::new();
        for instance in candidates {
            let profile = instance.identity().profile.clone();
            let mut state = instance.lifecycle.lock().await;
            if *state != InstanceLifecycle::Live {
                continue;
            }
            if now.saturating_sub(instance.last_used_ms.load(Ordering::SeqCst)) < idle_ms {
                continue;
            }
            // No page may be in flight, and none may slip in after this
            // check: both sides serialize on the lifecycle lock.
            if instance.page_count() != 0
                || instance.admission.available_permits() != self.config.max_pages_per_profile
            {
                continue;
            }
            self.pause_seam(&profile, "retire-decision").await;
            if *state != InstanceLifecycle::Live {
                continue;
            }
            *state = InstanceLifecycle::Retiring;
            self.remove_exact_instance(&instance);
            drop(state);
            stopped.push(profile);
            self.shutdown_instance(&instance).await;
        }
        stopped
    }

    /// Stop every browser (daemon teardown).
    pub async fn shutdown_all(&self) {
        let instances: Vec<Arc<BrowserInstance>> =
            self.browsers.lock().unwrap().values().cloned().collect();
        for instance in instances {
            self.shutdown_instance(&instance).await;
        }
    }

    /// Remove the map entry only when it still points at this exact
    /// instance (never evict a replacement launched in the meantime).
    fn remove_exact_instance(&self, instance: &Arc<BrowserInstance>) {
        let mut map = self.browsers.lock().unwrap();
        if let Some(existing) = map.get(&instance.id) {
            if Arc::ptr_eq(existing, instance) {
                map.remove(&instance.id);
            }
        }
    }

    async fn shutdown_instance(&self, instance: &Arc<BrowserInstance>) {
        {
            let mut state = instance.lifecycle.lock().await;
            if *state == InstanceLifecycle::Live {
                *state = InstanceLifecycle::Retiring;
            }
        }
        self.remove_exact_instance(instance);
        if let Some(task) = instance.event_task.lock().unwrap().take() {
            task.abort();
        }
        let pages = instance.pages_snapshot();
        for page in pages {
            let _ = page.close().await;
        }
        let _ = instance
            .client
            .send(
                None,
                "Browser.close",
                serde_json::json!({}),
                deadline_in(2_000),
                &CancellationToken::new(),
            )
            .await;
        let _ = instance.launched.kill(&self.supervisor, KILL_GRACE_MS);
        let _ = instance.client.close().await;
        instance.broker.shutdown().await;
        {
            let mut state = instance.lifecycle.lock().await;
            *state = InstanceLifecycle::Dead;
        }
        if let Some(incognito) = &instance.incognito {
            incognito.remove();
        }
    }

    fn drop_instance(&self, instance: &Arc<BrowserInstance>) {
        self.remove_exact_instance(instance);
        let _ = instance.launched.kill(&self.supervisor, KILL_GRACE_MS);
    }

    /// Retire every live browser for an identity — **both** modes — for
    /// operational reasons (egress change, profile reset). The next
    /// acquisition starts a fresh child.
    pub async fn retire(&self, identity: &BrowserIdentity) -> Result<(), BrowserError> {
        let ids: Vec<BrowserInstanceId> = self
            .browsers
            .lock()
            .unwrap()
            .keys()
            .filter(|id| id.identity == *identity)
            .cloned()
            .collect();
        if ids.is_empty() {
            return Err(BrowserError::Bound {
                detail: format!("no live browser for profile {}", identity.profile),
            });
        }
        for id in ids {
            let instance = self.browsers.lock().unwrap().get(&id).cloned();
            if let Some(instance) = instance {
                self.shutdown_instance(&instance).await;
            }
        }
        Ok(())
    }

    /// Retire one exact instance (mode-qualified). The next acquisition for
    /// that id starts a fresh child.
    pub async fn retire_instance(&self, id: &BrowserInstanceId) -> Result<(), BrowserError> {
        let instance = self.browsers.lock().unwrap().get(id).cloned();
        match instance {
            Some(instance) => {
                self.shutdown_instance(&instance).await;
                Ok(())
            }
            None => Err(BrowserError::Bound {
                detail: format!(
                    "no live {:?} browser for profile {}",
                    id.mode, id.identity.profile
                ),
            }),
        }
    }

    /// Clear a verification stop after a human completed the challenge in a
    /// dedicated headed profile.
    pub fn resume_profile(&self, profile: &str) -> Result<(), BrowserError> {
        validate_profile_name(profile)?;
        let mut resumed = false;
        for instance in self.browsers.lock().unwrap().values() {
            if instance.identity().profile == profile {
                *instance.stop.lock().unwrap() = None;
                resumed = true;
            }
        }
        if resumed {
            Ok(())
        } else {
            Err(BrowserError::Bound {
                detail: format!("no live browser for profile {profile}"),
            })
        }
    }

    /// Typed health for every live browser.
    pub fn health(&self) -> Vec<BrowserHealth> {
        self.browsers
            .lock()
            .unwrap()
            .values()
            .map(|instance| {
                let alive = instance.child_is_alive(&self.supervisor);
                let state = if !alive {
                    BrowserState::Crashed
                } else if instance.stop_signal().is_some() {
                    BrowserState::Stopped
                } else if instance.page_count() > 0 {
                    BrowserState::Active
                } else {
                    BrowserState::Idle
                };
                let accounting = instance.broker.accounting();
                BrowserHealth {
                    account: instance.identity().account.clone(),
                    profile: instance.identity().profile.clone(),
                    egress: instance.identity().egress.clone(),
                    mode: instance.id.mode,
                    pid: instance.launched.pid,
                    child_id: instance.launched.child_id,
                    state,
                    pages: instance.page_count(),
                    proxy_url: instance.broker.proxy_url(),
                    isolation: instance.launched.isolation.clone(),
                    last_used_ms: instance.last_used_ms.load(Ordering::SeqCst),
                    requests_total: accounting.requests_total,
                    blocked_total: accounting.blocked_total,
                }
            })
            .collect()
    }

    /// Live browser count (bounded by `max_browsers`).
    pub fn browser_count(&self) -> usize {
        self.browsers.lock().unwrap().len()
    }

    /// Every live instance for an identity, mode-qualified. Never an
    /// arbitrary single browser: a persistent and an incognito instance
    /// share the identity label and are distinct results.
    pub fn instances_for(&self, identity: &BrowserIdentity) -> Vec<BrowserInstanceId> {
        self.browsers
            .lock()
            .unwrap()
            .keys()
            .filter(|id| id.identity == *identity)
            .cloned()
            .collect()
    }

    /// The proxy URL(s) a live browser (or browsers) for this identity is
    /// using, mode-qualified and multi-valued.
    pub fn proxy_url_for(&self, identity: &BrowserIdentity) -> Vec<(BrowserInstanceId, String)> {
        self.browsers
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| id.identity == *identity)
            .map(|(id, instance)| (id.clone(), instance.broker.proxy_url()))
            .collect()
    }

    /// Page count for one exact (mode-qualified) instance.
    pub fn page_count_for(&self, id: &BrowserInstanceId) -> Option<usize> {
        self.browsers
            .lock()
            .unwrap()
            .get(id)
            .map(|instance| instance.page_count())
    }

    /// True when the exact instance's child is still registered live.
    pub fn is_live(&self, id: &BrowserInstanceId) -> bool {
        self.browsers
            .lock()
            .unwrap()
            .get(id)
            .map(|instance| instance.child_is_alive(&self.supervisor))
            .unwrap_or(false)
    }

    /// The process owner a browser child is registered under.
    pub fn owner_for(source: &str, identity: &BrowserIdentity) -> ProcessOwner {
        ProcessOwner::Browser {
            source: source.to_string(),
            profile: identity.profile.clone(),
        }
    }

    /// The source label a live browser was launched with, if any.
    pub fn source_for(&self, id: &BrowserInstanceId) -> Option<String> {
        self.browsers
            .lock()
            .unwrap()
            .get(id)
            .map(|instance| instance.source.clone())
    }

    /// Bounded central download records for one exact instance
    /// (diagnostics / tests): the GUID -> context/page tracking surface.
    pub fn download_records_for(
        &self,
        id: &BrowserInstanceId,
    ) -> Option<Vec<crate::download::DownloadRecord>> {
        self.browsers
            .lock()
            .unwrap()
            .get(id)
            .map(|instance| instance.downloads.records())
    }

    /// Download stats for one exact instance (diagnostics / tests).
    pub fn download_stats_for(
        &self,
        id: &BrowserInstanceId,
    ) -> Option<crate::download::DownloadStats> {
        self.browsers
            .lock()
            .unwrap()
            .get(id)
            .map(|instance| instance.downloads.stats())
    }

    /// Profile directory (diagnostics / tests).
    pub fn profile_dir(&self, profile: &str) -> Result<PathBuf, BrowserError> {
        self.profiles.profile_dir(profile)
    }

    /// The `PageState` of one page, for diagnostics.
    pub fn page_state(page: &Page) -> PageState {
        page.state()
    }

    /// Test/diagnostic hook: the current time as the manager sees it.
    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }
}

/// What `prepare_profiles` produced, with an explicit cleanup for the failed
/// launch paths (the temporary profile is removed, a persistent one is not).
struct PreparedProfiles {
    profile_dir: PathBuf,
    profile_rel: PathBuf,
    scratch_rel: PathBuf,
    incognito: Option<IncognitoProfile>,
}

impl PreparedProfiles {
    fn take_incognito(&mut self) -> Option<IncognitoProfile> {
        self.incognito.take()
    }

    fn cleanup(&mut self) {
        if let Some(incognito) = self.incognito.take() {
            incognito.remove();
        }
    }
}

/// The instance-level root-session event pump: browser-domain download
/// events (no session id) are owned centrally, and every native download is
/// denied through `Browser.cancelDownload` with a bounded deadline. The task
/// holds only a `Weak` instance and exits when the CDP broadcast closes.
fn spawn_root_event_pump(instance: &Arc<BrowserInstance>) -> tokio::task::JoinHandle<()> {
    let mut events = instance.client.subscribe();
    let weak: Weak<BrowserInstance> = Arc::downgrade(instance);
    tokio::spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "browser event pump lagged; events dropped");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let Some(instance) = weak.upgrade() else {
                break;
            };
            if event.session_id.is_some() {
                // Browser-domain events carry no session id; session-scoped
                // events belong to the per-page pumps.
                continue;
            }
            match event.method.as_str() {
                "Browser.downloadWillBegin" | "Browser.downloadProgress" => {
                    if let Some(denial) = instance
                        .downloads
                        .observe_native(&event.method, &event.params)
                    {
                        let _ = instance
                            .client
                            .send(
                                None,
                                "Browser.cancelDownload",
                                serde_json::json!({ "guid": denial.guid }),
                                deadline_in(3_000),
                                &CancellationToken::new(),
                            )
                            .await;
                    }
                }
                _ => {}
            }
        }
    })
}

impl Drop for BrowserManager {
    /// Daemon teardown backstop: whatever the explicit shutdown paths did
    /// not cover is killed synchronously by owner through the supervisor.
    fn drop(&mut self) {
        let instances: Vec<Arc<BrowserInstance>> = {
            let mut browsers = self.browsers.lock().unwrap();
            browsers.drain().map(|(_, instance)| instance).collect()
        };
        for instance in instances {
            let owner = ProcessOwner::Browser {
                source: instance.source.clone(),
                profile: instance.identity().profile.clone(),
            };
            let _ = self.supervisor.kill_all_for(owner);
        }
    }
}

/// A wall-clock default deadline for navigation, from config.
pub fn navigation_deadline(config: &BrowserConfig) -> Deadline {
    crate::timeutil::deadline_in(config.navigation_timeout_ms)
}

/// True when the current process clock has advanced past `idle_shutdown_s`
/// since `last_used_ms` (exposed for the daemon's maintenance tick).
pub fn idle_expired(config: &BrowserConfig, now_ms: i64, last_used_ms: i64) -> bool {
    now_ms.saturating_sub(last_used_ms) >= (config.idle_shutdown_s as i64).saturating_mul(1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_bounds_are_enforced() {
        let mut config = BrowserConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.idle_shutdown_s, 300);
        assert_eq!(config.max_browsers, 2);
        assert_eq!(config.max_pages_per_profile, 1);
        assert!(!config.downloads.enabled);
        config.max_browsers = 0;
        assert!(config.validate().is_err());
        config.max_browsers = 2;
        config.idle_shutdown_s = 0;
        assert!(config.validate().is_err());
        config.idle_shutdown_s = 300;
        config.max_pages_per_profile = 9;
        assert!(config.validate().is_err());
    }

    #[test]
    fn identities_are_strict() {
        assert!(BrowserIdentity::new("acct", "procurement-cn", "direct")
            .validate()
            .is_ok());
        for bad in ["../x", "A", "", "with space"] {
            assert!(
                BrowserIdentity::new("acct", bad, "direct")
                    .validate()
                    .is_err(),
                "{bad:?}"
            );
            assert!(
                BrowserIdentity::new("acct", "p1", bad).validate().is_err(),
                "{bad:?}"
            );
        }
        assert!(BrowserIdentity::new("", "p1", "direct").validate().is_err());
        assert!(BrowserIdentity::new("a\u{7}b", "p1", "direct")
            .validate()
            .is_err());
    }

    #[test]
    fn instance_ids_qualify_the_mode() {
        let identity = BrowserIdentity::new("acct", "p1", "direct");
        let persistent = BrowserInstanceId::persistent(identity.clone());
        let incognito = BrowserInstanceId::incognito(identity);
        assert_eq!(persistent.mode(), BrowserMode::Persistent);
        assert_eq!(incognito.mode(), BrowserMode::Incognito);
        assert_ne!(persistent, incognito);
        assert_ne!(persistent.key(), incognito.key());
        assert!(persistent.key().ends_with("persistent"));
        assert!(incognito.key().ends_with("incognito"));
    }

    #[test]
    fn idle_expiry_is_monotonic_and_bounded() {
        let config = BrowserConfig::default();
        assert!(!idle_expired(&config, 1_000, 1_000));
        assert!(idle_expired(&config, 301_000, 1_000));
        assert!(!idle_expired(&config, 299_999, 1_000));
    }

    #[test]
    fn invalid_download_policy_is_refused_by_config() {
        let mut config = BrowserConfig {
            enabled: true,
            ..BrowserConfig::default()
        };
        config.downloads.enabled = true;
        config.downloads.directory = Some("../outside".to_string());
        assert!(config.validate().is_err());
        config.downloads.directory = Some("downloads".to_string());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn disabled_config_is_a_typed_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(faktor_cas::Cas::open(tmp.path().join("cas")).unwrap());
        let supervisor = ProcessSupervisor::new(cas);
        let manager =
            BrowserManager::new(supervisor, BrowserConfig::default(), tmp.path()).unwrap();
        assert_eq!(manager.browser_count(), 0);
        assert!(manager.health().is_empty());
    }
}
