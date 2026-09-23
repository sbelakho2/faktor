//! Chromium launch through the workspace `ProcessSupervisor` (spec §9).
//!
//! There is exactly one way this crate starts a browser: a
//! `faktor_terminal::SpawnConfig` with `ProcessOwner::Browser { source,
//! profile }` and an environment built by `browser_env_spec` (never the
//! daemon environment). `std::process::Command` is never used here.
//!
//! The launch is proxy-only: `--proxy-server=http://127.0.0.1:<broker>` plus
//! `--proxy-bypass-list=<-loopback>` (so even loopback destinations go
//! through the broker), and extra arguments that could defeat either the
//! proxy or the control-plane are refused typed.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;

use faktor_core::cancellation::CancellationToken;
use faktor_core::time::Deadline;
use faktor_fs::RootedDir;
use faktor_terminal::{browser_env_spec, ProcessOwner, ProcessSupervisor, SpawnConfig};

use crate::error::BrowserError;
use crate::timeutil::{deadline_instant, now_ms};

/// Launch parameters for one browser child.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub executable: PathBuf,
    /// Headless (default in the runtime config).
    pub headless: bool,
    /// The `--user-data-dir` root (persistent profile dir or incognito dir).
    pub profile_dir: PathBuf,
    /// Scratch dir used for HOME/TMPDIR/XDG so Chromium never reads the
    /// operator's real home.
    pub scratch_dir: PathBuf,
    /// Anchored authority for the scratch base: every scratch directory is
    /// created through it, so a symlink-swapped scratch entry is refused
    /// instead of followed.
    pub scratch_root: RootedDir,
    /// The scratch base relative to `scratch_root`.
    pub scratch_rel: PathBuf,
    /// The local egress broker address — the ONLY egress Chromium gets.
    pub proxy_addr: std::net::SocketAddr,
    pub owner_source: String,
    pub owner_profile: String,
    /// Operational extra flags. Validated: flags that could redirect egress
    /// or the control plane are refused.
    pub extra_args: Vec<String>,
    /// How long to wait for the DevTools endpoint.
    pub launch_timeout_ms: u64,
}

/// Flags an extra-arg list may never contain: they would defeat the
/// proxy-only egress or the supervised control plane.
const FORBIDDEN_EXTRA_ARG_PREFIXES: &[&str] = &[
    "--proxy-server",
    "--no-proxy-server",
    "--proxy-bypass-list",
    "--proxy-pac-url",
    "--remote-debugging-port",
    "--remote-debugging-address",
    "--remote-debugging-pipe",
    "--user-data-dir",
    "--disable-web-security",
    "--ignore-certificate-errors",
    "--allow-running-insecure-content",
    "--disable-features",
    "--enable-features",
    // Sandbox removal is never an operational flag: a browser authority that
    // disables its own sandbox is refused typed (fail closed).
    "--no-sandbox",
];

/// Validate caller-supplied extra flags. A refused flag is a typed config
/// error — the browser is never launched with a weakened egress.
pub fn validate_extra_args(extra_args: &[String]) -> Result<(), BrowserError> {
    for arg in extra_args {
        if arg.contains('\n') || arg.contains('\0') {
            return Err(BrowserError::invalid_config(
                "extra browser arguments may not contain control characters",
            ));
        }
        let lower = arg.to_ascii_lowercase();
        if let Some(flag) = FORBIDDEN_EXTRA_ARG_PREFIXES
            .iter()
            .find(|flag| lower.starts_with(&lower_prefix(flag)))
        {
            return Err(BrowserError::invalid_config(format!(
                "extra browser argument {arg:?} would override {flag}; proxy-only and supervised \
                 debugging are not overridable"
            )));
        }
    }
    Ok(())
}

fn lower_prefix(flag: &str) -> String {
    flag.to_ascii_lowercase()
}

/// The exact argv this crate launches Chromium with. Public so tests (and
/// operators) can assert the proxy-only invariant.
pub fn chromium_args(options: &LaunchOptions) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--remote-debugging-port=0".to_string(),
        "--remote-debugging-address=127.0.0.1".to_string(),
        format!("--user-data-dir={}", options.profile_dir.display()),
        format!("--proxy-server=http://{}", options.proxy_addr),
        "--proxy-bypass-list=<-loopback>".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-component-update".to_string(),
        "--disable-sync".to_string(),
        "--disable-extensions".to_string(),
        "--disable-breakpad".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--disable-notifications".to_string(),
        "--metrics-recording-only".to_string(),
        "--no-service-autorun".to_string(),
        "--password-store=basic".to_string(),
        "--use-mock-keychain".to_string(),
        "--deny-permission-prompts".to_string(),
        "--mute-audio".to_string(),
        "--hide-scrollbars".to_string(),
        "--window-size=1280,800".to_string(),
        "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication"
            .to_string(),
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
        "--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
    ];
    if options.headless {
        args.push("--headless=new".to_string());
    }
    args.extend(options.extra_args.iter().cloned());
    args.push("about:blank".to_string());
    args
}

/// Resolve the Chromium executable: an explicit configured path, else the
/// well-known names on `PATH` and the macOS application bundles.
pub fn resolve_executable(configured: Option<&Path>) -> Result<PathBuf, BrowserError> {
    if let Some(path) = configured {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(BrowserError::BrowserUnavailable {
            detail: format!("configured chromium executable {path:?} is not a file"),
        });
    }
    let names = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "chrome.exe",
    ];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    for bundle in [
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ] {
        let candidate = PathBuf::from(bundle);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(BrowserError::BrowserUnavailable {
        detail: "no chromium executable configured and none found on PATH".to_string(),
    })
}

/// A supervised, launched browser child.
///
/// Ownership is explicit: an explicit [`LaunchedBrowser::kill`] (or
/// [`LaunchedBrowser::disarm`]) disarms the `Drop` backstop, so a launch
/// transaction that fails after the spawn can never leave a supervised
/// Chromium alive, even on an error path that forgot to kill it.
pub struct LaunchedBrowser {
    pub pid: u32,
    pub child_id: u64,
    pub owner: ProcessOwner,
    pub devtools_ws_url: String,
    /// Bounded tail of the child's stderr (diagnostics only).
    pub stderr_tail: Vec<String>,
    pub started_ms: i64,
    supervisor: Arc<ProcessSupervisor>,
    armed: AtomicBool,
}

impl std::fmt::Debug for LaunchedBrowser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchedBrowser")
            .field("pid", &self.pid)
            .field("child_id", &self.child_id)
            .field("owner", &self.owner)
            .field("started_ms", &self.started_ms)
            .field("armed", &self.armed.load(Ordering::SeqCst))
            .finish()
    }
}

impl LaunchedBrowser {
    pub fn is_alive(&self, supervisor: &ProcessSupervisor) -> bool {
        supervisor
            .alive()
            .iter()
            .any(|handle| handle.id == self.child_id)
    }

    /// Whole-tree kill through the supervisor (never a raw `kill`). Disarms
    /// the `Drop` backstop.
    pub fn kill(&self, supervisor: &ProcessSupervisor, grace_ms: u64) -> Result<(), BrowserError> {
        self.disarm();
        supervisor
            .kill(self.child_id, grace_ms)
            .map_err(|e| BrowserError::internal(format!("supervisor kill failed: {e}")))
    }

    /// Disarm the `Drop` backstop without killing (ownership was transferred
    /// to another explicit teardown path).
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

impl Drop for LaunchedBrowser {
    /// Backstop: a spawned child whose launch transaction failed must never
    /// outlive its `LaunchedBrowser`. The kill is the supervisor's bounded
    /// whole-tree terminate (never a raw signal), and every explicit path
    /// disarms first so this is a no-op there.
    fn drop(&mut self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.supervisor.kill(self.child_id, KILL_GRACE_MS);
        }
    }
}

const STDERR_TAIL_LINES: usize = 40;
const DEVTOOLS_PREFIX: &str = "DevTools listening on ws://";

/// Validate the DevTools endpoint Chromium announced on stderr. The endpoint
/// is attacker-influenced output, so it is parsed strictly and never handed
/// to a dialer verbatim:
///
/// * scheme must be exactly `ws`;
/// * no userinfo, no query, no fragment;
/// * the host must be a loopback **literal** (`127.0.0.0/8` or `::1`);
///   names such as `localhost` are refused (a name can be re-pointed between
///   validation and dial);
/// * the port must be explicit and non-zero;
/// * the path must be the expected DevTools browser path
///   (`/devtools/browser/<id>`).
///
/// Chromium legitimately announces exactly
/// `ws://127.0.0.1:<port>/devtools/browser/<uuid>`, which this accepts.
pub fn validate_devtools_ws_url(url: &str) -> Result<(), String> {
    let Some(rest) = url.strip_prefix("ws://") else {
        return Err("announced endpoint is not a ws:// URL".to_string());
    };
    if rest.contains('@') {
        return Err("announced endpoint carries userinfo".to_string());
    }
    if rest.contains('?') || rest.contains('#') {
        return Err("announced endpoint carries a query or fragment".to_string());
    }
    let Some((authority, path)) = rest.split_once('/') else {
        return Err("announced endpoint has no DevTools path".to_string());
    };
    let (host, port) = split_authority(authority)
        .ok_or_else(|| "announced endpoint has no explicit host:port".to_string())?;
    if port == 0 {
        return Err("announced endpoint uses port 0".to_string());
    }
    let address: std::net::IpAddr = host
        .parse()
        .map_err(|_| format!("announced endpoint host {host:?} is not an IP literal"))?;
    if !address.is_loopback() {
        return Err(format!(
            "announced endpoint host {host:?} is not a loopback literal"
        ));
    }
    let Some(id) = path.strip_prefix("devtools/browser/") else {
        return Err(format!(
            "announced endpoint path {path:?} is not a DevTools browser path"
        ));
    };
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("announced DevTools browser id is malformed".to_string());
    }
    Ok(())
}

/// Split `host:port` / `[v6]:port`; returns `None` without an explicit port.
fn split_authority(authority: &str) -> Option<(&str, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        return Some((host, port.parse().ok()?));
    }
    let (host, port) = authority.rsplit_once(':')?;
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some((host, port.parse().ok()?))
}

/// Launches Chromium through the supervisor and waits for its DevTools
/// endpoint.
pub struct ChromiumLauncher {
    supervisor: std::sync::Arc<ProcessSupervisor>,
}

impl ChromiumLauncher {
    pub fn new(supervisor: std::sync::Arc<ProcessSupervisor>) -> Self {
        Self { supervisor }
    }

    pub fn supervisor(&self) -> &std::sync::Arc<ProcessSupervisor> {
        &self.supervisor
    }

    /// Spawn the child and wait for `DevTools listening on ws://...`.
    pub async fn launch(
        &self,
        options: LaunchOptions,
        cancel: &CancellationToken,
    ) -> Result<LaunchedBrowser, BrowserError> {
        validate_extra_args(&options.extra_args)?;
        let args = chromium_args(&options);
        let env = browser_env_spec(browser_scratch_env(&options)?)?;
        let config = SpawnConfig {
            cmd: options.executable.to_string_lossy().to_string(),
            args,
            cwd: options.profile_dir.clone(),
            env,
            owner: ProcessOwner::Browser {
                source: options.owner_source.clone(),
                profile: options.owner_profile.clone(),
            },
            capture: false,
            ..SpawnConfig::default()
        };
        let spawned = self
            .supervisor
            .spawn_detached_with_pipes(config)
            .map_err(|e| BrowserError::BrowserUnavailable {
                detail: format!("supervisor refused the chromium spawn: {e}"),
            })?;
        let pid = spawned.child_pid;
        let child_id = self
            .supervisor
            .alive()
            .into_iter()
            .find(|handle| handle.pid == pid)
            .map(|handle| handle.id)
            .ok_or_else(|| {
                BrowserError::internal("spawned chromium is not in the supervisor registry")
            })?;
        let started_ms = now_ms();
        let deadline = Deadline::at(
            started_ms.saturating_add(options.launch_timeout_ms.min(i64::MAX as u64) as i64),
        );
        let stderr = tokio::process::ChildStderr::from_std(spawned.stderr)
            .map_err(|e| BrowserError::internal(format!("cannot adopt chromium stderr: {e}")))?;
        let mut stdout = tokio::process::ChildStdout::from_std(spawned.stdout)
            .map_err(|e| BrowserError::internal(format!("cannot adopt chromium stdout: {e}")))?;
        // Drain stdout so a chatty child can never block on a full pipe.
        // This task owns no application state (it discards bytes).
        tokio::spawn(async move {
            let mut sink = tokio::io::sink();
            let _ = tokio::io::copy(&mut stdout, &mut sink).await;
        });
        drop(spawned.stdin);
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut tail: Vec<String> = Vec::new();
        let mut line = String::new();
        let ws_url = loop {
            line.clear();
            let read = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.kill_quietly(child_id);
                    return Err(BrowserError::Cancelled);
                }
                result = tokio::time::timeout_at(deadline_instant(deadline), reader.read_line(&mut line)) => {
                    match result {
                        Ok(Ok(n)) => n,
                        Ok(Err(e)) => {
                            self.kill_quietly(child_id);
                            return Err(BrowserError::BrowserCrashed {
                                detail: format!("chromium stderr read failed: {e}"),
                            });
                        }
                        Err(_) => {
                            self.kill_quietly(child_id);
                            return Err(BrowserError::LaunchTimeout {
                                waited_ms: options.launch_timeout_ms,
                            });
                        }
                    }
                }
            };
            if read == 0 {
                let detail = tail.last().cloned().unwrap_or_else(|| {
                    "chromium exited before announcing its DevTools endpoint".to_string()
                });
                return Err(BrowserError::BrowserUnavailable {
                    detail: format!("chromium exited during launch: {detail}"),
                });
            }
            let trimmed = line.trim().to_string();
            if let Some(rest) = trimmed.strip_prefix(DEVTOOLS_PREFIX) {
                let url = format!("ws://{rest}");
                match validate_devtools_ws_url(&url) {
                    Ok(()) => break url,
                    Err(detail) => {
                        // The announced endpoint is hostile/malformed: kill
                        // the child and never dial. The raw URL is not
                        // echoed (it may carry attacker-chosen userinfo).
                        self.kill_quietly(child_id);
                        return Err(BrowserError::BrowserUnavailable {
                            detail: format!(
                                "chromium announced an unusable DevTools endpoint: {detail}"
                            ),
                        });
                    }
                }
            }
            if !trimmed.is_empty() {
                if tail.len() >= STDERR_TAIL_LINES {
                    tail.remove(0);
                }
                tail.push(trimmed);
            }
        };
        Ok(LaunchedBrowser {
            pid,
            child_id,
            owner: ProcessOwner::Browser {
                source: options.owner_source,
                profile: options.owner_profile,
            },
            devtools_ws_url: ws_url,
            stderr_tail: tail,
            started_ms,
            supervisor: self.supervisor.clone(),
            armed: AtomicBool::new(true),
        })
    }

    fn kill_quietly(&self, child_id: u64) {
        let _ = self.supervisor.kill(child_id, 2000);
    }
}

/// The exact environment entries the browser child gets on top of the
/// allowlist: a scratch HOME and TMPDIR plus XDG homes, all inside the
/// profile's scratch directory and all created through the anchored
/// [`RootedDir`] authority (a symlink swap fails typed instead of being
/// followed).
fn browser_scratch_env(options: &LaunchOptions) -> Result<Vec<(OsString, OsString)>, BrowserError> {
    let mut entries = Vec::new();
    let dirs: [(&str, &str); 5] = [
        ("HOME", ""),
        ("TMPDIR", "tmp"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "data"),
    ];
    for (name, sub) in dirs {
        let rel = if sub.is_empty() {
            options.scratch_rel.clone()
        } else {
            options.scratch_rel.join(sub)
        };
        options.scratch_root.create_dir_all(&rel).map_err(|e| {
            BrowserError::profile(format!("cannot create browser scratch dir: {e}"))
        })?;
        options
            .scratch_root
            .restrict_owner_only(&rel)
            .map_err(|e| {
                BrowserError::profile(format!("cannot restrict browser scratch dir: {e}"))
            })?;
        entries.push((
            OsString::from(name),
            OsString::from(options.scratch_root.join(&rel)),
        ));
    }
    Ok(entries)
}

/// A convenience deadline for launch waits.
pub fn launch_deadline(timeout_ms: u64) -> Deadline {
    crate::timeutil::deadline_in(timeout_ms)
}

/// Grace period used when killing a browser tree.
pub const KILL_GRACE_MS: u64 = 2000;

/// Small helper so tests can assert a child is really gone.
pub fn wait_for_exit(supervisor: &ProcessSupervisor, pid: u32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if !supervisor.pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    !supervisor.pid_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> LaunchOptions {
        let tmp = tempfile::tempdir().unwrap();
        let scratch_root = RootedDir::create(tmp.path()).unwrap();
        LaunchOptions {
            executable: PathBuf::from("/bin/true"),
            headless: true,
            profile_dir: PathBuf::from("/tmp/kp-profile"),
            scratch_dir: PathBuf::from("/tmp/kp-scratch"),
            scratch_root,
            scratch_rel: PathBuf::from("scratch"),
            proxy_addr: "127.0.0.1:34567".parse().unwrap(),
            owner_source: "example-source".to_string(),
            owner_profile: "procurement-cn".to_string(),
            extra_args: Vec::new(),
            launch_timeout_ms: 1000,
        }
    }

    #[test]
    fn argv_is_proxy_only_and_forces_loopback_through_the_broker() {
        let args = chromium_args(&options());
        assert!(args
            .iter()
            .any(|a| a == "--proxy-server=http://127.0.0.1:34567"));
        assert!(args.iter().any(|a| a == "--proxy-bypass-list=<-loopback>"));
        assert!(args
            .iter()
            .any(|a| a == "--remote-debugging-address=127.0.0.1"));
        assert!(args
            .iter()
            .any(|a| a.starts_with("--user-data-dir=/tmp/kp-profile")));
        assert!(!args.iter().any(|a| a == "--no-proxy-server"));
        assert!(!args.iter().any(|a| a == "--headless"));
        assert!(args.iter().any(|a| a == "--headless=new"));
        assert_eq!(args.last().unwrap(), "about:blank");
    }

    #[test]
    fn extra_args_cannot_override_egress_or_control_plane() {
        for bad in [
            "--no-proxy-server",
            "--proxy-server=http://evil:1",
            "--proxy-bypass-list=*",
            "--remote-debugging-port=9222",
            "--user-data-dir=/tmp/other",
            "--disable-web-security",
            "--ignore-certificate-errors",
            "--disable-features=WebRtcHideLocalIpsWithMdns",
            "--no-sandbox",
        ] {
            let err = validate_extra_args(&[bad.to_string()])
                .expect_err("a hostile extra arg must be refused typed");
            assert_eq!(err.code(), "invalid_config", "{bad}");
        }
        validate_extra_args(&["--disable-gpu".to_string()])
            .expect("benign operational flags stay allowed");
    }

    #[test]
    fn configured_executable_is_used_and_missing_is_typed() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("chromium");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_executable(Some(&exe)).unwrap(), exe);
        let err = resolve_executable(Some(&tmp.path().join("missing"))).unwrap_err();
        assert_eq!(err.code(), "browser_unavailable");
    }

    #[test]
    fn devtools_endpoint_validation_only_accepts_literal_loopback() {
        assert!(validate_devtools_ws_url("ws://127.0.0.1:9222/devtools/browser/abc-123").is_ok());
        assert!(validate_devtools_ws_url("ws://127.8.9.10:1/devtools/browser/a").is_ok());
        assert!(validate_devtools_ws_url("ws://[::1]:9222/devtools/browser/abc").is_ok());
        for hostile in [
            // Scheme.
            "wss://127.0.0.1:9222/devtools/browser/x",
            "http://127.0.0.1:9222/devtools/browser/x",
            // Non-literal loopback or non-loopback literal.
            "ws://localhost:9222/devtools/browser/x",
            "ws://attacker.example:9222/devtools/browser/x",
            "ws://128.0.0.1:9222/devtools/browser/x",
            "ws://[::2]:9222/devtools/browser/x",
            // Userinfo / query / fragment.
            "ws://user:pass@127.0.0.1:9222/devtools/browser/x",
            "ws://127.0.0.1:9222/devtools/browser/x?y=1",
            "ws://127.0.0.1:9222/devtools/browser/x#frag",
            // Ports.
            "ws://127.0.0.1:0/devtools/browser/x",
            "ws://127.0.0.1/devtools/browser/x",
            "ws://[::1]/devtools/browser/x",
            // Paths.
            "ws://127.0.0.1:9222/devtools/page/x",
            "ws://127.0.0.1:9222/",
            "ws://127.0.0.1:9222/devtools/browser/",
            "ws://127.0.0.1:9222/devtools/browser/a/b",
        ] {
            assert!(
                validate_devtools_ws_url(hostile).is_err(),
                "must be refused: {hostile}"
            );
        }
    }
}
