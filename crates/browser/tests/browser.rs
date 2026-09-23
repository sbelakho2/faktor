//! Adversarial integration tests for the browser authority.
//!
//! Everything runs offline: Chromium is the crate's `fake_chromium` fixture
//! binary (minimal CDP over a loopback WebSocket, driven by a JSON scenario
//! file and journaling every command). Real TCP is used only for the local
//! egress broker/origin servers on 127.0.0.1.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use faktor_browser::{
    BrowserConfig, BrowserError, BrowserIdentity, BrowserManager, DestinationPolicy, HostPattern,
    PagePurpose, ProfileStore, UpstreamProxy, VerificationKind,
};
use faktor_core::cancellation::CancellationToken;
use faktor_core::time::{Deadline, SystemClock};
use faktor_terminal::{ProcessOwner, ProcessSupervisor};

fn fixture_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_chromium"))
}

fn deadline_in(ms: u64) -> Deadline {
    Deadline::now_plus(&SystemClock, ms)
}

struct Harness {
    _dir: tempfile::TempDir,
    supervisor: Arc<ProcessSupervisor>,
    manager: Arc<BrowserManager>,
    journal: PathBuf,
    dump: PathBuf,
}

impl Harness {
    async fn new(scenario: Value, tune: impl FnOnce(&mut BrowserConfig)) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let scenario_path = dir.path().join("scenario.json");
        std::fs::write(&scenario_path, serde_json::to_vec(&scenario).unwrap()).unwrap();
        let journal = dir.path().join("journal.jsonl");
        let dump = dir.path().join("launch.json");
        let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
        let supervisor = ProcessSupervisor::new(cas);
        let mut config = BrowserConfig {
            enabled: true,
            executable: Some(fixture_exe()),
            launch_timeout_ms: 15_000,
            ..BrowserConfig::default()
        };
        config.extra_args = vec![
            format!("--fake-scenario={}", scenario_path.display()),
            format!("--fake-journal={}", journal.display()),
            format!("--fake-dump-launch={}", dump.display()),
        ];
        tune(&mut config);
        config.validate().expect("test config must be valid");
        let manager = BrowserManager::new(supervisor.clone(), config, dir.path()).expect("manager");
        Harness {
            _dir: dir,
            supervisor,
            manager,
            journal,
            dump,
        }
    }

    fn identity(profile: &str) -> BrowserIdentity {
        BrowserIdentity::new("acct", profile, "direct")
    }

    fn policy() -> DestinationPolicy {
        DestinationPolicy::first_party_only(vec![
            HostPattern::parse("127.0.0.1").unwrap(),
            HostPattern::parse("first.test").unwrap(),
        ])
    }

    async fn acquire(&self, profile: &str) -> Result<faktor_browser::Page, BrowserError> {
        self.manager
            .acquire_page(
                "1688",
                &Self::identity(profile),
                Self::policy(),
                &PagePurpose::new("extraction"),
                deadline_in(15_000),
                &CancellationToken::new(),
            )
            .await
    }

    fn journal_entries(&self) -> Vec<Value> {
        read_journal(&self.journal)
    }

    fn journal_count(&self, dir: &str, method: &str) -> usize {
        self.journal_entries()
            .iter()
            .filter(|entry| {
                entry.get("dir").and_then(Value::as_str) == Some(dir)
                    && entry.get("method").and_then(Value::as_str) == Some(method)
            })
            .count()
    }

    fn launch_dump(&self) -> Value {
        let raw = std::fs::read(&self.dump).expect("launch dump exists");
        serde_json::from_slice(&raw).expect("launch dump is json")
    }

    fn browser_children(&self) -> Vec<faktor_terminal::ChildHandle> {
        self.supervisor
            .alive()
            .into_iter()
            .filter(|handle| matches!(handle.owner, ProcessOwner::Browser { .. }))
            .collect()
    }
}

fn read_journal(path: &Path) -> Vec<Value> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// Poll the supervisor for a reaped child (the reaper thread races the
/// assertion otherwise).
fn wait_for_reap(
    supervisor: &ProcessSupervisor,
    pid: u32,
    timeout: Duration,
) -> Option<faktor_terminal::Reaped> {
    let start = std::time::Instant::now();
    loop {
        if let Some(entry) = supervisor.reap().into_iter().find(|entry| entry.pid == pid) {
            return Some(entry);
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------- launch/env

#[tokio::test]
async fn launched_env_has_no_planted_secrets_and_is_proxy_only() {
    const SECRETS: &[(&str, &str)] = &[
        ("OPENAI_API_KEY", "sk-planted-openai-secret"),
        ("ANTHROPIC_API_KEY", "sk-ant-planted"),
        ("FAKTOR_SERVER_PASSWORD", "planted-server-password"),
        ("FAKTOR_DIGIKEY_CLIENT_SECRET", "planted-digikey-secret"),
        ("PROXY_PASSWORD", "planted-proxy-password"),
        ("SOME_AUTH_TOKEN", "planted-token"),
    ];
    for (name, value) in SECRETS {
        std::env::set_var(name, value);
    }
    let harness = Harness::new(json!({}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    let _ = page.close().await;
    let dump = harness.launch_dump();
    for (name, value) in SECRETS {
        std::env::remove_var(name);
        let leaked = dump["env"].get(*name).is_some()
            || serde_json::to_string(&dump["env"]).unwrap().contains(value);
        assert!(!leaked, "secret {name} leaked into the browser environment");
    }
    // The environment is the allowlist plus the exact scratch entries.
    let env = dump["env"].as_object().expect("env object");
    let allowed = [
        "PATH",
        "LANG",
        "LC_ALL",
        "TZ",
        "HOME",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "GIT_TERMINAL_PROMPT",
    ];
    for key in env.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "unexpected inherited variable in the browser environment: {key}"
        );
    }
    // HOME is the scratch dir, never the operator's real home.
    let scratch = env["HOME"].as_str().unwrap();
    assert!(
        scratch.contains("scratch"),
        "HOME must be the scratch dir: {scratch}"
    );

    // Proxy-only argv: exactly one proxy-server, forced loopback bypass.
    let argv: Vec<String> = dump["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let proxy_flags: Vec<&String> = argv
        .iter()
        .filter(|arg| arg.starts_with("--proxy-server="))
        .collect();
    assert_eq!(proxy_flags.len(), 1, "exactly one --proxy-server: {argv:?}");
    assert!(
        proxy_flags[0].starts_with("--proxy-server=http://127.0.0.1:"),
        "the only configured egress must be the loopback broker: {proxy_flags:?}"
    );
    assert!(argv.iter().any(|a| a == "--proxy-bypass-list=<-loopback>"));
    assert!(argv
        .iter()
        .any(|a| a == "--remote-debugging-address=127.0.0.1"));
    assert!(!argv.iter().any(|a| a == "--no-proxy-server"));

    // The broker that port belongs to is real and enforcing: it is the same
    // port the manager reports.
    let proxy_url = harness.manager.health()[0].proxy_url.clone();
    assert_eq!(proxy_flags[0], &format!("--proxy-server={proxy_url}"));
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn proxy_only_cannot_be_overridden_by_extra_args() {
    let err = faktor_browser::launch::validate_extra_args(&["--no-proxy-server".to_string()])
        .expect_err("--no-proxy-server must be refused");
    assert_eq!(err.code(), "invalid_config");
    let err =
        faktor_browser::launch::validate_extra_args(&["--proxy-server=http://evil:1".to_string()])
            .expect_err("--proxy-server override must be refused");
    assert_eq!(err.code(), "invalid_config");
    // A config carrying such a flag is refused before any process exists.
    let mut config = BrowserConfig {
        enabled: true,
        executable: Some(fixture_exe()),
        ..BrowserConfig::default()
    };
    config.extra_args = vec!["--no-proxy-server".to_string()];
    assert!(config.validate().is_err());
    // The launch argv still carries exactly one loopback proxy flag.
    let argv = faktor_browser::chromium_args(&faktor_browser::LaunchOptions {
        executable: fixture_exe(),
        headless: true,
        profile_dir: PathBuf::from("/tmp/x"),
        scratch_dir: PathBuf::from("/tmp/x/scratch"),
        proxy_addr: "127.0.0.1:1".parse().unwrap(),
        owner_source: "1688".to_string(),
        owner_profile: "p1".to_string(),
        extra_args: vec!["--disable-gpu".to_string()],
        launch_timeout_ms: 1000,
    });
    assert_eq!(
        argv.iter()
            .filter(|arg| arg.starts_with("--proxy-server="))
            .count(),
        1
    );
    assert!(argv
        .iter()
        .any(|arg| arg == "--proxy-server=http://127.0.0.1:1"));
}

// --------------------------------------------------------------- e2e flows

#[tokio::test]
async fn navigation_and_network_records_flow_through_cdp() {
    let harness = Harness::new(
        json!({
            "page_url": "https://first.test/product",
            "network": [
                {"request_id": "x1", "url": "https://first.test/api.json", "resource_type": "XHR",
                 "status": 200, "mime_type": "application/json", "body": "{\"ok\":true}"}
            ]
        }),
        |_| {},
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    let outcome = page
        .navigate(
            "https://first.test/product",
            deadline_in(10_000),
            &CancellationToken::new(),
        )
        .await
        .expect("navigate");
    assert_eq!(outcome.url, "https://first.test/product");
    assert_eq!(outcome.lifecycle, faktor_browser::Lifecycle::Loaded);
    let network = page.network();
    assert_eq!(network.len(), 1);
    assert_eq!(network[0].request_id, "x1");
    assert_eq!(network[0].status, Some(200));
    assert_eq!(network[0].mime_type.as_deref(), Some("application/json"));
    // No Fetch interception was enabled for a paused request here, so no
    // decisions were taken; the interception suite covers the drop path.
    assert_eq!(page.interceptor_stats().paused_total, 0);
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn interception_drops_blocked_resource_types_and_tracking_hosts() {
    let harness = Harness::new(
        json!({
            "pause_requests": true,
            "network": [
                {"request_id": "img", "url": "https://first.test/hero.png", "resource_type": "Image"},
                {"request_id": "xhr", "url": "https://first.test/api.json", "resource_type": "XHR"},
                {"request_id": "track", "url": "https://tracker.example/pixel.js", "resource_type": "Script"},
                {"request_id": "doc", "url": "https://first.test/page", "resource_type": "Document"}
            ]
        }),
        |_| {},
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/page",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    let stats = page.interceptor_stats();
    assert_eq!(stats.paused_total, 4);
    assert_eq!(stats.continued_total, 2, "XHR and Document continue");
    assert_eq!(stats.blocked_total, 2, "Image and tracking host blocked");
    assert_eq!(harness.journal_count("recv", "Fetch.enable"), 1);
    assert_eq!(harness.journal_count("recv", "Fetch.failRequest"), 2);
    assert_eq!(harness.journal_count("recv", "Fetch.continueRequest"), 2);
    // The Fetch.enable pattern intercepts the request stage.
    let enable = harness
        .journal_entries()
        .into_iter()
        .find(|entry| entry["method"] == "Fetch.enable")
        .unwrap();
    assert_eq!(enable["params"]["patterns"][0]["requestStage"], "Request");
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn body_capture_is_bounded_and_oversize_is_refused() {
    let harness = Harness::new(
        json!({
            "body_bytes": 6000,
            "network": [
                {"request_id": "big", "url": "https://first.test/big.json", "resource_type": "XHR",
                 "status": 200, "mime_type": "application/json"},
                {"request_id": "huge", "url": "https://first.test/huge.json", "resource_type": "XHR",
                 "status": 200, "mime_type": "application/json", "body": "A".repeat(20_000)}
            ]
        }),
        |config| {
            config.capture.max_body_bytes = 4096;
            config.capture.hard_max_body_bytes = 8192;
        },
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/big.json",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    let captured = page
        .capture_body("big", 4096, deadline_in(10_000), &CancellationToken::new())
        .await
        .expect("capture");
    assert!(captured.truncated);
    assert_eq!(captured.byte_len, 6000);
    assert_eq!(captured.bytes.len(), 4096, "soft cap enforced");
    // 20_000 bytes of base64 exceeds the 8KiB hard cap: refused before
    // decoding, never materialized.
    let refused = page
        .capture_body("huge", 4096, deadline_in(10_000), &CancellationToken::new())
        .await;
    assert_eq!(refused.err().map(|e| e.code()), Some("response_too_large"));
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn navigation_cancellation_stops_loading_and_releases_the_page() {
    let harness = Harness::new(json!({"suppress_lifecycle": true}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let page_clone = page.clone();
    let navigation = tokio::spawn(async move {
        page_clone
            .navigate(
                "https://first.test/slow",
                deadline_in(30_000),
                &cancel_clone,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    cancel.cancel();
    let result = navigation.await.unwrap();
    assert_eq!(result, Err(BrowserError::Cancelled));
    assert!(
        page.is_closed(),
        "the page must be released on cancellation"
    );
    assert_eq!(
        harness.manager.page_count_for(&Harness::identity("p1")),
        Some(0)
    );
    assert_eq!(harness.journal_count("recv", "Page.stopLoading"), 1);
    assert_eq!(harness.journal_count("recv", "Target.closeTarget"), 1);
    // The profile itself stays healthy: a new page can be acquired.
    let replacement = harness.acquire("p1").await.expect("profile stays healthy");
    let _ = replacement.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn repeated_cancelled_captures_never_wedge_the_profile() {
    let harness = Harness::new(json!({"suppress_lifecycle": true}), |config| {
        config.max_pages_per_profile = 1;
    })
    .await;
    for round in 0..5 {
        let page = harness.acquire("p1").await.expect("acquire");
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let page_clone = page.clone();
        let navigation = tokio::spawn(async move {
            page_clone
                .navigate(
                    "https://first.test/slow",
                    deadline_in(30_000),
                    &cancel_clone,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        cancel.cancel();
        let result = navigation.await.unwrap();
        assert_eq!(result, Err(BrowserError::Cancelled), "round {round}");
        assert_eq!(
            harness.manager.page_count_for(&Harness::identity("p1")),
            Some(0),
            "round {round}: the slot must be released"
        );
    }
    // The bound was never the thing that failed: a final acquisition works.
    let page = harness.acquire("p1").await.expect("profile never wedged");
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn abandoned_in_flight_page_releases_its_slot_on_drop() {
    let harness = Harness::new(json!({"suppress_lifecycle": true}), |config| {
        config.max_pages_per_profile = 1;
    })
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    let page_clone = page.clone();
    let navigation = tokio::spawn(async move {
        page_clone
            .navigate(
                "https://first.test/slow",
                deadline_in(30_000),
                &CancellationToken::new(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    // The caller abandons the whole capture: the future is dropped mid-await
    // without a chance to call `close`.
    drop(page);
    navigation.abort();
    let _ = navigation.await;
    assert_eq!(
        harness.manager.page_count_for(&Harness::identity("p1")),
        Some(0),
        "an abandoned page must release the slot on Drop"
    );
    // The profile is immediately usable again.
    let replacement = harness.acquire("p1").await.expect("slot freed");
    let _ = replacement.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn idle_shutdown_reaps_browsers_whose_pages_were_abandoned() {
    let clock = Arc::new(faktor_core::time::TestClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let scenario_path = dir.path().join("scenario.json");
    std::fs::write(&scenario_path, b"{}").unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    let supervisor = ProcessSupervisor::new(cas);
    let config = BrowserConfig {
        enabled: true,
        executable: Some(fixture_exe()),
        idle_shutdown_s: 1,
        ..BrowserConfig::default()
    };
    let manager =
        BrowserManager::with_clock(supervisor.clone(), config, dir.path(), clock.clone()).unwrap();
    let page = manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
        .expect("acquire");
    assert_eq!(manager.page_count_for(&Harness::identity("p1")), Some(1));
    drop(page);
    assert_eq!(
        manager.page_count_for(&Harness::identity("p1")),
        Some(0),
        "the abandoned page released its slot"
    );
    clock.advance(1_500);
    assert!(
        !manager.shutdown_idle().await.is_empty(),
        "a browser with only abandoned pages must be reaped"
    );
    assert!(
        supervisor.alive().is_empty(),
        "no orphan after idle shutdown"
    );
    manager.shutdown_all().await;
}

#[tokio::test]
async fn concurrent_acquisitions_never_exceed_the_page_bound() {
    let harness = Harness::new(json!({}), |config| {
        config.max_pages_per_profile = 1;
        config.max_browsers = 1;
    })
    .await;
    // All eight acquisitions race; every page that is admitted stays OPEN in
    // the result vector until the assertions run, so the refusal is about
    // the bound and not about a dropped handle.
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    let results: Arc<std::sync::Mutex<Vec<Result<faktor_browser::Page, BrowserError>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    for _ in 0..8 {
        let manager = harness.manager.clone();
        let barrier = barrier.clone();
        let results = results.clone();
        tokio::spawn(async move {
            let result = manager
                .acquire_page(
                    "1688",
                    &Harness::identity("p1"),
                    Harness::policy(),
                    &PagePurpose::new("extraction"),
                    deadline_in(15_000),
                    &CancellationToken::new(),
                )
                .await;
            results.lock().unwrap().push(result);
            barrier.wait().await;
        });
    }
    tokio::time::timeout(Duration::from_secs(15), barrier.wait())
        .await
        .expect("all acquisitions settled");
    let (opened, refused) = {
        let guard = results.lock().unwrap();
        let mut opened = 0usize;
        let mut refused = 0usize;
        for result in guard.iter() {
            match result {
                Ok(_) => opened += 1,
                Err(error) => {
                    assert_eq!(error.code(), "bound", "{error:?}");
                    refused += 1;
                }
            }
        }
        (opened, refused)
    };
    assert_eq!(opened, 1, "exactly one admission wins");
    assert_eq!(refused, 7, "every other acquisition is a typed refusal");
    // Exactly one target was created: the check and the open were serialized.
    assert_eq!(harness.journal_count("recv", "Target.createTarget"), 1);
    assert_eq!(
        harness.manager.page_count_for(&Harness::identity("p1")),
        Some(1)
    );
    drop(results);
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn incognito_never_reuses_a_persistent_page() {
    let harness = Harness::new(json!({}), |config| {
        config.max_browsers = 2;
    })
    .await;
    let persistent = harness.acquire("p1").await.expect("persistent page");
    let incognito = harness
        .manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            Harness::policy(),
            &PagePurpose::incognito("probe"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
        .expect("incognito page");
    // Two distinct browsers: a temporary context was created instead of
    // reusing the persistent profile.
    let health = harness.manager.health();
    assert_eq!(harness.manager.browser_count(), 2);
    assert_eq!(health.len(), 2);
    assert_eq!(health.iter().filter(|entry| entry.pages == 1).count(), 2);
    let pids: std::collections::BTreeSet<u32> = health.iter().map(|entry| entry.pid).collect();
    assert_eq!(pids.len(), 2, "two distinct browser processes: {health:?}");
    // The latest launch (the incognito one) used a temporary context and a
    // temporary profile directory.
    assert_eq!(
        harness.journal_count("recv", "Target.createBrowserContext"),
        1
    );
    let created: Vec<Value> = harness
        .journal_entries()
        .into_iter()
        .filter(|entry| entry["method"] == "Target.createTarget")
        .collect();
    assert!(!created.is_empty());
    assert_eq!(
        created
            .iter()
            .filter(|entry| entry["params"].get("browserContextId").is_some())
            .count(),
        1,
        "the incognito target carries a browser context: {created:?}"
    );
    let argv: Vec<String> = harness.launch_dump()["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect();
    assert!(
        argv.iter()
            .any(|arg| arg.starts_with("--user-data-dir=") && arg.contains(".incognito")),
        "the incognito launch must not reuse the persistent profile dir: {argv:?}"
    );
    // The persistent profile directory was not turned into a context.
    assert!(!harness
        .manager
        .profile_dir("probe")
        .unwrap()
        .join("Default")
        .exists());
    let _ = persistent.close().await;
    let _ = incognito.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn idle_shutdown_kills_the_child_and_leaves_no_orphan() {
    let clock = Arc::new(faktor_core::time::TestClock::new(1_000_000));
    let dir = tempfile::tempdir().unwrap();
    let scenario_path = dir.path().join("scenario.json");
    std::fs::write(&scenario_path, b"{}").unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    let supervisor = ProcessSupervisor::new(cas);
    let config = BrowserConfig {
        enabled: true,
        executable: Some(fixture_exe()),
        idle_shutdown_s: 1,
        ..BrowserConfig::default()
    };
    let manager =
        BrowserManager::with_clock(supervisor.clone(), config, dir.path(), clock.clone()).unwrap();
    let page = manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
        .expect("acquire");
    let pid = manager.health()[0].pid;
    // A browser with an open page is never idle-killed, however long the
    // caller's clock says it has been unused.
    clock.advance(60_000);
    assert!(manager.shutdown_idle().await.is_empty());
    assert_eq!(
        supervisor.alive().len(),
        1,
        "open page keeps the child alive"
    );
    let _ = page.close().await;
    clock.advance(1_500);
    assert!(!manager.shutdown_idle().await.is_empty(), "must shut down");
    assert!(
        supervisor.alive().is_empty(),
        "no live child after idle shutdown"
    );
    assert!(faktor_browser::launch::wait_for_exit(
        &supervisor,
        pid,
        Duration::from_secs(5)
    ));
    assert_eq!(manager.browser_count(), 0);
    // A fresh acquisition starts a new child (lazy startup).
    let again = manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
        .expect("relaunch");
    assert_ne!(manager.health()[0].pid, pid);
    let _ = again.close().await;
    manager.shutdown_all().await;
}

#[tokio::test]
async fn crash_detection_is_typed_and_leaves_no_zombie() {
    let harness = Harness::new(json!({"exit_on_method": "Page.navigate"}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    let pid = harness.manager.health()[0].pid;
    let result = page
        .navigate(
            "https://first.test/boom",
            deadline_in(10_000),
            &CancellationToken::new(),
        )
        .await;
    match result {
        Err(BrowserError::BrowserCrashed { .. }) => {}
        other => panic!("expected BrowserCrashed, got {other:?}"),
    }
    // The supervisor reaped the child: no zombie, exit code visible.
    let entry =
        wait_for_reap(&harness.supervisor, pid, Duration::from_secs(5)).expect("child reaped");
    assert_eq!(entry.exit_code, Some(9));
    assert!(harness.browser_children().is_empty());
    assert!(
        !harness.supervisor.pid_alive(pid),
        "crashed child must not linger"
    );
    // The instance is reported crashed and a new acquisition surfaces the
    // typed crash (then forgets the dead instance).
    assert_eq!(
        harness.manager.health()[0].state,
        faktor_browser::BrowserState::Crashed
    );
    let again = harness.acquire("p1").await;
    assert!(matches!(again, Err(BrowserError::BrowserCrashed { .. })));
    assert_eq!(harness.manager.browser_count(), 0);
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn verification_detection_stops_the_profile_until_resumed() {
    let harness = Harness::new(json!({"verification": {"captcha": true}}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/challenge",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    let guard = page
        .guard_verification(deadline_in(10_000), &CancellationToken::new())
        .await;
    assert_eq!(
        guard,
        Err(BrowserError::VerificationRequired {
            kind: VerificationKind::Captcha
        })
    );
    // Automated work for the profile is stopped: a new page is refused with
    // the same typed state, never silently retried.
    let refused = harness.acquire("p1").await;
    assert_eq!(
        refused.err(),
        Some(BrowserError::VerificationRequired {
            kind: VerificationKind::Captcha
        })
    );
    // A human completes the challenge, the operator resumes the profile.
    harness.manager.resume_profile("p1").unwrap();
    let resumed = harness.acquire("p1").await.expect("resume");
    let _ = resumed.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn login_form_detection_is_authentication_required() {
    let harness = Harness::new(json!({"verification": {"login_form": true}}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/login",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    let guard = page
        .guard_verification(deadline_in(10_000), &CancellationToken::new())
        .await;
    assert_eq!(guard, Err(BrowserError::AuthenticationRequired));
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn downloads_are_disabled_by_default_and_cancelled() {
    let harness = Harness::new(
        json!({
            "download_will_begin": {
                "guid": "g1",
                "url": "https://first.test/file.pdf",
                "suggestedFilename": "file.pdf"
            }
        }),
        |_| {},
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/file.pdf",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    assert_eq!(
        page.last_download_error(),
        Some(BrowserError::DownloadBlocked {
            url: "https://first.test/file.pdf".to_string()
        })
    );
    let behavior = harness
        .journal_entries()
        .into_iter()
        .find(|entry| entry["method"] == "Browser.setDownloadBehavior")
        .expect("download behavior installed");
    assert_eq!(behavior["params"]["behavior"], "deny");
    assert_eq!(harness.journal_count("recv", "Browser.cancelDownload"), 1);
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn screenshots_only_on_explicit_request() {
    let harness = Harness::new(json!({}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/page",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    assert_eq!(harness.journal_count("recv", "Page.captureScreenshot"), 0);
    let shot = page
        .screenshot("png", deadline_in(10_000), &CancellationToken::new())
        .await
        .expect("screenshot");
    assert_eq!(shot.bytes, b"fake-png-bytes");
    assert_eq!(harness.journal_count("recv", "Page.captureScreenshot"), 1);
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

// ------------------------------------------------------------- profiles/bounds

#[tokio::test]
async fn persistent_profiles_are_owner_only_and_survive_shutdown() {
    let harness = Harness::new(json!({}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    let _ = page.close().await;
    let profile_dir = harness.manager.profile_dir("p1").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&profile_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "profile dir must be 0700");
        let scratch = profile_dir.join("scratch");
        let mode = std::fs::metadata(&scratch).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "scratch dir must be 0700");
    }
    // Cookies are confined to the profile directory (never CAS, never
    // model-visible): the cookie store path lives inside it.
    let cookies = harness.manager.profiles().cookie_store_path("p1").unwrap();
    assert!(cookies.starts_with(&profile_dir), "{cookies:?}");
    // Chromium was pointed at the profile dir.
    let argv: Vec<String> = harness.launch_dump()["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(argv
        .iter()
        .any(|arg| arg == &format!("--user-data-dir={}", profile_dir.display())));
    harness.manager.shutdown_all().await;
    assert!(profile_dir.exists(), "persistent profile survives shutdown");
    // No orphan and no live browser after teardown.
    assert!(harness.browser_children().is_empty());
}

#[tokio::test]
async fn daemon_teardown_kills_every_browser_tree() {
    let harness = Harness::new(json!({}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    let pid = harness.manager.health()[0].pid;
    drop(page);
    let supervisor = harness.supervisor.clone();
    let manager = harness.manager.clone();
    drop(harness);
    assert!(
        supervisor.alive().iter().any(|handle| handle.pid == pid),
        "browser is live before teardown"
    );
    drop(manager);
    assert!(
        faktor_browser::launch::wait_for_exit(&supervisor, pid, Duration::from_secs(5)),
        "manager drop must kill the whole browser tree"
    );
    assert!(supervisor.alive().is_empty(), "no orphan after teardown");
}

#[tokio::test]
async fn incognito_profiles_are_temporary() {
    let harness = Harness::new(json!({}), |_| {}).await;
    let page = harness
        .manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            Harness::policy(),
            &PagePurpose::incognito("probe"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
        .expect("incognito acquire");
    let argv: Vec<String> = harness.launch_dump()["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let user_data = argv
        .iter()
        .find(|arg| arg.starts_with("--user-data-dir="))
        .expect("user-data-dir");
    let dir = PathBuf::from(user_data.trim_start_matches("--user-data-dir="));
    assert!(dir.to_string_lossy().contains(".incognito"), "{dir:?}");
    assert!(dir.exists());
    // Incognito context created, persistent profile untouched.
    assert_eq!(
        harness.journal_count("recv", "Target.createBrowserContext"),
        1
    );
    assert!(!harness
        .manager
        .profile_dir("probe")
        .unwrap()
        .join("Default")
        .exists());
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
    assert!(!dir.exists(), "incognito dir removed on shutdown");
}

#[tokio::test]
async fn page_and_browser_bounds_are_typed_refusals() {
    let harness = Harness::new(json!({}), |config| {
        config.max_pages_per_profile = 1;
        config.max_browsers = 1;
    })
    .await;
    let page = harness.acquire("p1").await.expect("first page");
    let second = harness.acquire("p1").await;
    assert_eq!(second.err().map(|e| e.code()), Some("bound"));
    // A second profile needs a second browser: refused at max_browsers.
    let other = harness.acquire("p2").await;
    assert_eq!(other.err().map(|e| e.code()), Some("bound"));
    let _ = page.close().await;
    let after = harness.acquire("p1").await.expect("slot freed");
    let _ = after.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn identity_egress_is_stable_and_changes_only_on_retire() {
    let harness = Harness::new(json!({}), |_| {}).await;
    let page = harness.acquire("p1").await.expect("acquire");
    let pid = harness.manager.health()[0].pid;
    let _ = page.close().await;
    let again = harness.acquire("p1").await.expect("reuse");
    assert_eq!(harness.manager.health()[0].pid, pid, "identity is stable");
    assert_eq!(
        harness
            .manager
            .source_for(&Harness::identity("p1"))
            .as_deref(),
        Some("1688")
    );
    let _ = again.close().await;
    harness
        .manager
        .retire(&Harness::identity("p1"))
        .await
        .unwrap();
    let relaunched = harness.acquire("p1").await.expect("relaunch");
    assert_ne!(harness.manager.health()[0].pid, pid, "retire relaunches");
    let _ = relaunched.close().await;

    // A changed destination policy for the same live identity is refused.
    let page = harness.acquire("p1").await.expect("acquire");
    let changed = harness
        .manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            DestinationPolicy::first_party_only(vec![HostPattern::parse("other.test").unwrap()]),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(changed.err().map(|e| e.code()), Some("invalid_config"));
    let _ = page.close().await;

    // An unregistered egress route refuses before any process exists.
    let unknown = harness
        .manager
        .acquire_page(
            "1688",
            &BrowserIdentity::new("acct", "p9", "missing-route"),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(unknown.err().map(|e| e.code()), Some("egress_unavailable"));
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn disabled_browser_authority_refuses_without_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    let supervisor = ProcessSupervisor::new(cas);
    let manager =
        BrowserManager::new(supervisor.clone(), BrowserConfig::default(), dir.path()).unwrap();
    let result = manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(5_000),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result.err(), Some(BrowserError::Disabled));
    assert!(supervisor.alive().is_empty());
    assert_eq!(manager.browser_count(), 0);
}

// ---------------------------------------------------------------- static scan

#[test]
fn no_anti_bot_bypass_subsystem_exists_in_the_sources() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack = vec![root];
    let mut sources = String::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
                sources.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    for forbidden in [
        "capsolver",
        "2captcha",
        "anticaptcha",
        "deathbycaptcha",
        "puppeteer-extra",
        "undetected-chromedriver",
        "fingerprint_spoof",
        "spoof_fingerprint",
        "stealth_plugin",
        "rotate_proxy_ip",
    ] {
        assert!(
            !sources.to_ascii_lowercase().contains(forbidden),
            "anti-bot bypass marker found: {forbidden}"
        );
    }
}

#[test]
fn crate_has_no_site_knowledge_and_no_model_dependencies() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // No model/reasoning runtime or provider adapter may appear in the
    // dependency graph (spec §2).
    let cargo = std::fs::read_to_string(manifest.join("Cargo.toml")).unwrap();
    for forbidden in [
        "faktor-agent",
        "faktor-router",
        "faktor-openai",
        "faktor-anthropic",
        "faktor-google",
        "faktor-ollama",
        "faktor-provider",
        "reqwest",
    ] {
        assert!(
            !cargo.contains(forbidden),
            "faktor-browser must not depend on {forbidden}"
        );
    }
    // No marketplace vocabulary in production sources: site knowledge
    // belongs to connector crates only.
    let mut stack = vec![manifest.join("src")];
    let mut sources = String::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
                sources.push_str(&std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let lower = sources.to_ascii_lowercase();
    for site in ["1688", "alibaba", "mouser", "digikey", "lcsc"] {
        assert!(
            !lower.contains(site),
            "site knowledge marker found in faktor-browser sources: {site}"
        );
    }
}

#[tokio::test]
async fn profile_store_refuses_traversal_names() {
    let dir = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(dir.path().join("profiles")).unwrap();
    assert!(store.profile_dir("../escape").is_err());
    assert!(store.wipe("../../etc").is_err());
    assert!(store.cookie_store_path("bad/name").is_err());
}

#[tokio::test]
async fn upstream_route_selection_is_operational_config() {
    let harness = Harness::new(json!({}), |_| {}).await;
    harness
        .manager
        .register_egress(
            "upstream-eu",
            faktor_browser::UpstreamSelector::new(Some(UpstreamProxy::new("127.0.0.1", 9))),
        )
        .unwrap();
    let page = harness
        .manager
        .acquire_page(
            "1688",
            &BrowserIdentity::new("acct", "p1", "upstream-eu"),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
        .expect("acquire through a registered route");
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}
