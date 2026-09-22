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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::{Clock, Deadline, SystemClock};
use faktor_terminal::{ProcessOwner, ProcessSupervisor};

use crate::capture::CaptureLimits;
use crate::cdp::{CdpClient, CdpConfig};
use crate::download::DownloadPolicy;
use crate::egress::{
    BrokerConfig, BrokerHandle, DestinationPolicy, EgressBroker, UpstreamSelector,
};
use crate::error::BrowserError;
use crate::interception::Interceptor;
use crate::launch::{
    resolve_executable, ChromiumLauncher, LaunchOptions, LaunchedBrowser, KILL_GRACE_MS,
};
use crate::page::{Page, PageHost, PageState, VerificationSignal};
use crate::profile::{validate_profile_name, IncognitoProfile, ProfileStore, MAX_ACCOUNT_BYTES};

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
        self.capture.validate()?;
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
    pub pid: u32,
    pub child_id: u64,
    pub state: BrowserState,
    pub pages: usize,
    pub proxy_url: String,
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

/// A live browser instance.
pub struct BrowserInstance {
    source: String,
    identity: BrowserIdentity,
    launched: LaunchedBrowser,
    client: CdpClient,
    broker: BrokerHandle,
    #[allow(dead_code)] // retained for diagnostics; the profile store is the accessor
    profile_dir: PathBuf,
    #[allow(dead_code)]
    incognito: Option<IncognitoProfile>,
    context_id: Option<String>,
    policy: DestinationPolicy,
    pages: Mutex<HashMap<String, Page>>,
    stop: Mutex<Option<VerificationSignal>>,
    crashed: AtomicBool,
    last_used_ms: AtomicI64,
    clock: Arc<dyn Clock>,
}

impl BrowserInstance {
    fn touch(&self) {
        self.last_used_ms
            .store(self.clock.now_ms(), Ordering::SeqCst);
    }

    fn stop_signal(&self) -> Option<VerificationSignal> {
        *self.stop.lock().unwrap()
    }

    fn pages_snapshot(&self) -> Vec<Page> {
        self.pages.lock().unwrap().values().cloned().collect()
    }

    fn page_count(&self) -> usize {
        self.pages.lock().unwrap().len()
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
                    self.identity.profile
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
                    self.launched.pid, self.identity.profile
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
            profile = %self.identity.profile,
            signal = signal.kind().as_str(),
            "browser profile stopped: human verification required"
        );
        // Release the profile's page slots immediately (bounded work), then
        // close the targets in the background.
        let pages: Vec<Page> = {
            let mut map = self.pages.lock().unwrap();
            map.drain().map(|(_, page)| page).collect()
        };
        for page in pages {
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
    browsers: Mutex<HashMap<String, Arc<BrowserInstance>>>,
    egress_routes: Mutex<HashMap<String, UpstreamSelector>>,
    create_serial: tokio::sync::Mutex<()>,
    clock: Arc<dyn Clock>,
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
        }))
    }

    pub fn config(&self) -> &BrowserConfig {
        &self.config
    }

    pub fn profiles(&self) -> &ProfileStore {
        &self.profiles
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
        if let Err(error) = instance.check_alive(&self.supervisor) {
            self.drop_instance(&instance);
            return Err(error);
        }
        if let Some(signal) = instance.stop_signal() {
            return Err(signal.error());
        }
        if instance.page_count() >= self.config.max_pages_per_profile {
            return Err(BrowserError::bound(format!(
                "profile {} already has {} open page(s); max_pages_per_profile is {}",
                identity.profile,
                instance.page_count(),
                self.config.max_pages_per_profile
            )));
        }
        let page = self
            .open_page(&instance, &policy, deadline, cancel)
            .await
            .map_err(|error| {
                // A failed page open on a dead browser must not leave the
                // instance behind.
                if matches!(error, BrowserError::BrowserCrashed { .. }) {
                    self.drop_instance(&instance);
                }
                error
            })?;
        instance.touch();
        Ok(page)
    }

    async fn open_page(
        &self,
        instance: &Arc<BrowserInstance>,
        policy: &DestinationPolicy,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Page, BrowserError> {
        let client = instance.client.clone();
        let mut create_params = serde_json::json!({ "url": "about:blank" });
        if let Some(context_id) = &instance.context_id {
            create_params["browserContextId"] = serde_json::json!(context_id);
        }
        let created = client
            .send(None, "Target.createTarget", create_params, deadline, cancel)
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BrowserError::cdp("Target.createTarget returned no targetId"))?
            .to_string();
        let attached = match client
            .send(
                None,
                "Target.attachToTarget",
                serde_json::json!({ "targetId": target_id, "flatten": true }),
                deadline,
                cancel,
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let _ = client
                    .send(
                        None,
                        "Target.closeTarget",
                        serde_json::json!({ "targetId": target_id }),
                        crate::timeutil::deadline_in(2_000),
                        &CancellationToken::new(),
                    )
                    .await;
                return Err(error);
            }
        };
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
            crate::interception::InterceptionPolicy::new(policy.clone()),
        );
        let (enable_method, enable_params) = interceptor.enable_command();
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
        let page = Page::attach(
            client,
            target_id.clone(),
            session_id,
            interceptor,
            self.config.downloads.clone(),
            self.config.capture.clone(),
            crate::network::NetworkLimits {
                max_records: self.config.capture.max_network_records,
                max_body_bytes: self.config.capture.max_body_bytes,
                hard_max_body_bytes: self.config.capture.hard_max_body_bytes,
            },
            host,
        );
        instance
            .pages
            .lock()
            .unwrap()
            .insert(target_id, page.clone());
        Ok(page)
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
        let key = identity.key();
        if let Some(instance) = self.browsers.lock().unwrap().get(&key).cloned() {
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
            .launch_instance(source, identity, policy, purpose, deadline, cancel)
            .await?;
        self.browsers.lock().unwrap().insert(key, instance.clone());
        Ok(instance)
    }

    async fn launch_instance(
        &self,
        source: &str,
        identity: &BrowserIdentity,
        policy: &DestinationPolicy,
        purpose: &PagePurpose,
        deadline: Deadline,
        cancel: &CancellationToken,
    ) -> Result<Arc<BrowserInstance>, BrowserError> {
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
        let broker = EgressBroker::start(BrokerConfig {
            bind: "127.0.0.1:0".parse().expect("loopback literal"),
            policy: policy.clone(),
            upstream,
            ..BrokerConfig::default()
        })
        .await?;
        let executable = resolve_executable(self.config.executable.as_deref())?;
        let (profile_dir, incognito, scratch_dir) = if purpose.incognito {
            let incognito = self.profiles.incognito(&identity.profile)?;
            let dir = incognito.dir().to_path_buf();
            let scratch = dir.join("scratch");
            std::fs::create_dir_all(&scratch).map_err(|e| {
                BrowserError::profile(format!("cannot create incognito scratch: {e}"))
            })?;
            crate::profile::restrict_dir(&scratch)?;
            (dir, Some(incognito), scratch)
        } else {
            let dir = self.profiles.profile_dir(&identity.profile)?;
            let scratch = self.profiles.scratch_dir(&identity.profile)?;
            (dir, None, scratch)
        };
        self.config.downloads.validate(&profile_dir)?;
        self.config.downloads.prepare()?;
        let launch = self
            .launcher
            .launch(
                LaunchOptions {
                    executable,
                    headless: self.config.headless,
                    profile_dir: profile_dir.clone(),
                    scratch_dir,
                    proxy_addr: broker.addr(),
                    owner_source: source.to_string(),
                    owner_profile: identity.profile.clone(),
                    extra_args: self.config.extra_args.clone(),
                    launch_timeout_ms: self.config.launch_timeout_ms,
                },
                cancel,
            )
            .await;
        let launched = match launch {
            Ok(launched) => launched,
            Err(error) => {
                broker.shutdown().await;
                return Err(error);
            }
        };
        let connect = CdpClient::connect(
            &launched.devtools_ws_url,
            cancel,
            deadline,
            CdpConfig {
                max_message_bytes: self.config.capture.max_cdp_message_bytes,
                event_capacity: 2048,
            },
        )
        .await;
        let client = match connect {
            Ok(client) => client,
            Err(error) => {
                let _ = launched.kill(&self.supervisor, KILL_GRACE_MS);
                broker.shutdown().await;
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
            let _ = launched.kill(&self.supervisor, KILL_GRACE_MS);
            let _ = client.close().await;
            broker.shutdown().await;
            return Err(error);
        }
        let (behavior_method, behavior_params) = self.config.downloads.set_behavior_command();
        if let Err(error) = client
            .send(None, behavior_method, behavior_params, deadline, cancel)
            .await
        {
            let _ = launched.kill(&self.supervisor, KILL_GRACE_MS);
            let _ = client.close().await;
            broker.shutdown().await;
            return Err(error);
        }
        let context_id = if purpose.incognito {
            let created = client
                .send(
                    None,
                    "Target.createBrowserContext",
                    serde_json::json!({}),
                    deadline,
                    cancel,
                )
                .await?;
            Some(
                created
                    .get("browserContextId")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| BrowserError::cdp("Target.createBrowserContext returned no id"))?
                    .to_string(),
            )
        } else {
            None
        };
        Ok(Arc::new(BrowserInstance {
            source: source.to_string(),
            identity: identity.clone(),
            launched,
            client,
            broker,
            profile_dir,
            incognito,
            context_id,
            policy: policy.clone(),
            pages: Mutex::new(HashMap::new()),
            stop: Mutex::new(None),
            crashed: AtomicBool::new(false),
            last_used_ms: AtomicI64::new(self.clock.now_ms()),
            clock: self.clock.clone(),
        }))
    }

    /// Shut down every browser unused for at least `idle_shutdown_s` **and
    /// with no open page** (a browser mid-work is never killed by the idle
    /// path). Returns the profiles that were stopped. The whole tree is
    /// killed through the supervisor, so no orphan survives.
    pub async fn shutdown_idle(&self) -> Vec<String> {
        let now = self.clock.now_ms();
        let idle_ms = (self.config.idle_shutdown_s as i64).saturating_mul(1000);
        let victims: Vec<Arc<BrowserInstance>> = self
            .browsers
            .lock()
            .unwrap()
            .values()
            .filter(|instance| {
                instance.page_count() == 0
                    && now.saturating_sub(instance.last_used_ms.load(Ordering::SeqCst)) >= idle_ms
            })
            .cloned()
            .collect();
        let mut stopped = Vec::new();
        for instance in victims {
            stopped.push(instance.identity.profile.clone());
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

    async fn shutdown_instance(&self, instance: &Arc<BrowserInstance>) {
        self.browsers
            .lock()
            .unwrap()
            .remove(&instance.identity.key());
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
                crate::timeutil::deadline_in(2_000),
                &CancellationToken::new(),
            )
            .await;
        let _ = instance.launched.kill(&self.supervisor, KILL_GRACE_MS);
        let _ = instance.client.close().await;
        instance.broker.shutdown().await;
    }

    fn drop_instance(&self, instance: &Arc<BrowserInstance>) {
        self.browsers
            .lock()
            .unwrap()
            .remove(&instance.identity.key());
        let _ = instance.launched.kill(&self.supervisor, KILL_GRACE_MS);
    }

    /// Retire a live browser for operational reasons (egress change, profile
    /// reset). The next acquisition starts a fresh child.
    pub async fn retire(&self, identity: &BrowserIdentity) -> Result<(), BrowserError> {
        let instance = self.browsers.lock().unwrap().get(&identity.key()).cloned();
        match instance {
            Some(instance) => {
                self.shutdown_instance(&instance).await;
                Ok(())
            }
            None => Err(BrowserError::Bound {
                detail: format!("no live browser for profile {}", identity.profile),
            }),
        }
    }

    /// Clear a verification stop after a human completed the challenge in a
    /// dedicated headed profile.
    pub fn resume_profile(&self, profile: &str) -> Result<(), BrowserError> {
        validate_profile_name(profile)?;
        let mut resumed = false;
        for instance in self.browsers.lock().unwrap().values() {
            if instance.identity.profile == profile {
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
                    account: instance.identity.account.clone(),
                    profile: instance.identity.profile.clone(),
                    egress: instance.identity.egress.clone(),
                    pid: instance.launched.pid,
                    child_id: instance.launched.child_id,
                    state,
                    pages: instance.page_count(),
                    proxy_url: instance.broker.proxy_url(),
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

    /// The proxy URL a live browser for this identity is using, if any.
    pub fn proxy_url_for(&self, identity: &BrowserIdentity) -> Option<String> {
        self.browsers
            .lock()
            .unwrap()
            .get(&identity.key())
            .map(|instance| instance.broker.proxy_url())
    }

    /// Page count for a live browser.
    pub fn page_count_for(&self, identity: &BrowserIdentity) -> Option<usize> {
        self.browsers
            .lock()
            .unwrap()
            .get(&identity.key())
            .map(|instance| instance.page_count())
    }

    /// True when the instance's child is still registered live.
    pub fn is_live(&self, identity: &BrowserIdentity) -> bool {
        self.browsers
            .lock()
            .unwrap()
            .get(&identity.key())
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
    pub fn source_for(&self, identity: &BrowserIdentity) -> Option<String> {
        self.browsers
            .lock()
            .unwrap()
            .get(&identity.key())
            .map(|instance| instance.source.clone())
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
                profile: instance.identity.profile.clone(),
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
    fn idle_expiry_is_monotonic_and_bounded() {
        let config = BrowserConfig::default();
        assert!(!idle_expired(&config, 1_000, 1_000));
        assert!(idle_expired(&config, 301_000, 1_000));
        assert!(!idle_expired(&config, 299_999, 1_000));
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
