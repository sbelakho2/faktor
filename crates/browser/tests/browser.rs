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

    fn instance(profile: &str) -> faktor_browser::BrowserInstanceId {
        faktor_browser::BrowserInstanceId::persistent(Self::identity(profile))
    }

    fn incognito_instance(profile: &str) -> faktor_browser::BrowserInstanceId {
        faktor_browser::BrowserInstanceId::incognito(Self::identity(profile))
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
    let scratch_tmp = tempfile::tempdir().unwrap();
    let argv = faktor_browser::chromium_args(&faktor_browser::LaunchOptions {
        executable: fixture_exe(),
        headless: true,
        profile_dir: PathBuf::from("/tmp/x"),
        scratch_dir: PathBuf::from("/tmp/x/scratch"),
        scratch_root: faktor_fs::RootedDir::create(scratch_tmp.path()).unwrap(),
        scratch_rel: PathBuf::from("scratch"),
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
    let network = page.network().expect("complete history");
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
        harness.manager.page_count_for(&Harness::instance("p1")),
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
            harness.manager.page_count_for(&Harness::instance("p1")),
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
        harness.manager.page_count_for(&Harness::instance("p1")),
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
    assert_eq!(manager.page_count_for(&Harness::instance("p1")), Some(1));
    drop(page);
    assert_eq!(
        manager.page_count_for(&Harness::instance("p1")),
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
        harness.manager.page_count_for(&Harness::instance("p1")),
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
            .source_for(&Harness::instance("p1"))
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

// ------------------------------------------------------- event loss safety

#[tokio::test]
async fn critical_event_overflow_fails_the_page_instead_of_continuing() {
    // A burst of paused requests far beyond the critical queue capacity:
    // the fake browser sends every event before it reads the broker's
    // continue/fail replies, so the queue overflows deterministically.
    let mut items = Vec::new();
    for index in 0..64 {
        items.push(json!({
            "request_id": format!("r{index}"),
            "url": format!("https://first.test/{index}.json"),
            "resource_type": "XHR"
        }));
    }
    let harness = Harness::new(
        json!({"pause_requests": true, "network": items}),
        |config| {
            config.cdp_critical_event_capacity = 2;
        },
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    let result = page
        .navigate(
            "https://first.test/page",
            deadline_in(10_000),
            &CancellationToken::new(),
        )
        .await;
    match result {
        Err(BrowserError::EventStreamLagged { skipped }) => assert!(skipped >= 1),
        other => panic!("expected EventStreamLagged, got {other:?}"),
    }
    // The page failed loudly and released its slot: no paused request is
    // left waiting on a page nobody will serve.
    assert!(page.is_closed(), "a lagged page must be failed");
    assert_eq!(
        harness.manager.page_count_for(&Harness::instance("p1")),
        Some(0),
        "the failed page's slot is released"
    );
    // Complete-history queries refuse typed, never answer from partial data.
    assert!(matches!(
        page.network(),
        Err(BrowserError::EventStreamLagged { .. })
    ));
    assert_eq!(
        wait_for_journal_count(&harness, "Target.closeTarget", 1, Duration::from_secs(2)).await,
        1,
        "the failed page's target must be torn down"
    );
    harness.manager.shutdown_all().await;
}

async fn wait_for_journal_count(
    harness: &Harness,
    method: &str,
    expected: usize,
    timeout: Duration,
) -> usize {
    let start = std::time::Instant::now();
    loop {
        let count = harness.journal_count("recv", method);
        if count >= expected || start.elapsed() > timeout {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn observation_gap_latches_and_history_refuses() {
    // Observation fan-out capacity 1: while the pump awaits the reply to the
    // first critical `Fetch.requestPaused` (the fake browser sends every
    // event before it reads commands), the network observation events
    // overflow the lossy broadcast. The gap must latch and complete-history
    // queries must refuse; unlike a critical overflow the page stays usable.
    let mut items = Vec::new();
    for index in 0..64 {
        items.push(json!({
            "request_id": format!("r{index}"),
            "url": format!("https://first.test/{index}.json"),
            "resource_type": "XHR"
        }));
    }
    let harness = Harness::new(
        json!({"pause_requests": true, "network": items}),
        |config| {
            config.cdp_event_capacity = 1;
        },
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    let _ = page
        .navigate(
            "https://first.test/page",
            deadline_in(10_000),
            &CancellationToken::new(),
        )
        .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match page.network() {
            Err(BrowserError::EventStreamLagged { skipped }) => {
                assert!(skipped >= 1);
                break;
            }
            Err(other) => panic!("unexpected network error: {other:?}"),
            Ok(_) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the observation gap never latched"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
    assert!(
        !page.is_closed(),
        "an observation gap is incomplete history, not a failed page"
    );
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn hostile_devtools_endpoint_is_rejected_and_the_child_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-chromium.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'DevTools listening on ws://attacker.example:9222/devtools/browser/evil' >&2\nsleep 60\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cas = Arc::new(faktor_cas::Cas::open(dir.path().join("cas")).unwrap());
    let supervisor = ProcessSupervisor::new(cas);
    let launcher = faktor_browser::ChromiumLauncher::new(supervisor.clone());
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let error = launcher
        .launch(
            faktor_browser::LaunchOptions {
                executable: script,
                headless: true,
                profile_dir: profile,
                scratch_dir: dir.path().join("scratch"),
                scratch_root: faktor_fs::RootedDir::create(dir.path()).unwrap(),
                scratch_rel: PathBuf::from("scratch"),
                proxy_addr: "127.0.0.1:1".parse().unwrap(),
                owner_source: "1688".to_string(),
                owner_profile: "p1".to_string(),
                extra_args: Vec::new(),
                launch_timeout_ms: 3_000,
            },
            &CancellationToken::new(),
        )
        .await
        .expect_err("a hostile announced endpoint must be refused");
    assert_eq!(error.code(), "browser_unavailable", "{error:?}");
    // The child was killed: the supervisor holds no browser child and no
    // dial was ever made (launch never returned an endpoint).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !supervisor.alive().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        supervisor.alive().is_empty(),
        "the hostile child must be killed, no orphan survives"
    );
}

// ===========================================================================
// Lifecycle / containment wave: transactional open, launch rollback, idle
// races, instance identity, download hard caps and rooted containment.
// ===========================================================================

use faktor_browser::{DownloadPolicy, LifecycleSeam};

/// The pid the fake Chromium journaled at startup (the launch entry).
fn launch_pid(harness: &Harness) -> u32 {
    harness
        .journal_entries()
        .into_iter()
        .find(|entry| entry["dir"] == "launch")
        .and_then(|entry| entry["pid"].as_u64())
        .expect("launch journal entry with pid") as u32
}

/// The broker proxy port and temp profile dir the fake was launched with.
fn launch_proxy_and_profile(harness: &Harness) -> (u16, PathBuf) {
    let argv: Vec<String> = harness.launch_dump()["argv"]
        .as_array()
        .expect("argv")
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect();
    let proxy = argv
        .iter()
        .find(|arg| arg.starts_with("--proxy-server=http://127.0.0.1:"))
        .expect("proxy arg");
    let port: u16 = proxy
        .trim_start_matches("--proxy-server=http://127.0.0.1:")
        .parse()
        .unwrap();
    let user_data = argv
        .iter()
        .find(|arg| arg.starts_with("--user-data-dir="))
        .expect("user-data-dir arg");
    (
        port,
        PathBuf::from(user_data.trim_start_matches("--user-data-dir=")),
    )
}

fn broker_port_refuses_connections(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(250),
    )
    .is_err()
}

// ---------------------------------------------------- 1. transactional open

#[tokio::test]
async fn open_page_init_failures_close_the_created_target_at_every_step() {
    // (label, scenario, expected closeTarget count, target was handed to us)
    let cases: Vec<(&str, Value, usize, bool)> = vec![
        (
            "Target.createTarget refuses",
            json!({"fail_methods": ["Target.createTarget"]}),
            0,
            false,
        ),
        (
            "Target.attachToTarget fails",
            json!({"fail_methods": ["Target.attachToTarget"]}),
            1,
            true,
        ),
        (
            "attach-without-session",
            json!({"omit_result_fields": {"Target.attachToTarget": ["sessionId"]}}),
            1,
            true,
        ),
        (
            "Page.enable",
            json!({"fail_methods": ["Page.enable"]}),
            1,
            true,
        ),
        (
            "Network.enable",
            json!({"fail_methods": ["Network.enable"]}),
            1,
            true,
        ),
        (
            "Runtime.enable",
            json!({"fail_methods": ["Runtime.enable"]}),
            1,
            true,
        ),
        (
            "Fetch.enable",
            json!({"fail_methods": ["Fetch.enable"]}),
            1,
            true,
        ),
        (
            "createTarget-without-id",
            json!({"omit_result_fields": {"Target.createTarget": ["targetId"]}}),
            0,
            false,
        ),
    ];
    for (label, scenario, expected_closes, target_owned) in cases {
        let harness = Harness::new(scenario, |_| {}).await;
        let result = harness.acquire("p1").await;
        assert!(
            result.is_err(),
            "{label}: the injected failure must fail the open"
        );
        let created = harness.journal_count("recv", "Target.createTarget");
        let closed = harness.journal_count("recv", "Target.closeTarget");
        assert_eq!(
            created, 1,
            "{label}: one target was created (target count baseline is one)"
        );
        assert_eq!(
            closed, expected_closes,
            "{label}: a created-but-uninitialized target must be closed exactly once"
        );
        if target_owned {
            // Target count and manager page count are back at the baseline.
            assert_eq!(
                created - closed,
                0,
                "{label}: no leaked target may survive the failed open"
            );
        }
        assert_eq!(
            harness
                .manager
                .page_count_for(&Harness::instance("p1"))
                .unwrap_or(0),
            0,
            "{label}: no page may be recorded"
        );
        // The instance is not wedged: the browser is supervised and idle.
        assert_eq!(harness.manager.browser_count(), 1, "{label}");
        assert_eq!(harness.browser_children().len(), 1, "{label}");
        harness.manager.shutdown_all().await;
        assert!(
            harness.browser_children().is_empty(),
            "{label}: teardown leaves no orphan"
        );
    }
}

// ------------------------------------------------------ 2. launch rollback

#[tokio::test]
async fn failed_launch_rolls_back_child_broker_and_temp_profile() {
    let cases: Vec<(&str, Value)> = vec![
        (
            "Browser.getVersion error",
            json!({"fail_methods": ["Browser.getVersion"]}),
        ),
        (
            "child dies on Browser.getVersion",
            json!({"exit_on_method": "Browser.getVersion"}),
        ),
        (
            "Browser.setDownloadBehavior error",
            json!({"fail_methods": ["Browser.setDownloadBehavior"]}),
        ),
        (
            "Target.createBrowserContext error",
            json!({"fail_methods": ["Target.createBrowserContext"]}),
        ),
        (
            "Target.createBrowserContext without id",
            json!({"omit_result_fields": {"Target.createBrowserContext": ["browserContextId"]}}),
        ),
    ];
    for (label, scenario) in cases {
        let harness = Harness::new(scenario, |_| {}).await;
        let result = harness
            .manager
            .acquire_page(
                "1688",
                &Harness::identity("p1"),
                Harness::policy(),
                &PagePurpose::incognito("probe"),
                deadline_in(15_000),
                &CancellationToken::new(),
            )
            .await;
        assert!(
            result.is_err(),
            "{label}: the injected failure must surface"
        );
        assert_eq!(harness.manager.browser_count(), 0, "{label}");
        assert_eq!(
            harness
                .manager
                .page_count_for(&Harness::incognito_instance("p1")),
            None,
            "{label}"
        );
        // No orphan: the supervised child exits.
        let pid = launch_pid(&harness);
        assert!(
            faktor_browser::launch::wait_for_exit(&harness.supervisor, pid, Duration::from_secs(5)),
            "{label}: the rolled-back child must die"
        );
        assert!(harness.browser_children().is_empty(), "{label}");
        // The broker is stopped: its loopback port refuses connections.
        let (port, profile_dir) = launch_proxy_and_profile(&harness);
        assert!(
            broker_port_refuses_connections(port),
            "{label}: the egress broker must be shut down"
        );
        // The temporary incognito profile is gone.
        assert!(
            !profile_dir.exists(),
            "{label}: the temporary profile must be removed"
        );
        assert!(
            profile_dir.to_string_lossy().contains(".incognito"),
            "{label}: rollback test uses an incognito profile"
        );
    }
}

#[tokio::test]
async fn failed_launch_keeps_a_persistent_profile_but_removes_no_orphan() {
    let harness = Harness::new(json!({"fail_methods": ["Browser.getVersion"]}), |_| {}).await;
    let result = harness
        .manager
        .acquire_page(
            "1688",
            &Harness::identity("p1"),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    let pid = launch_pid(&harness);
    assert!(faktor_browser::launch::wait_for_exit(
        &harness.supervisor,
        pid,
        Duration::from_secs(5)
    ));
    let (port, profile_dir) = launch_proxy_and_profile(&harness);
    assert!(broker_port_refuses_connections(port));
    assert!(
        profile_dir.exists(),
        "a persistent profile is durable state and survives a failed launch"
    );
}

// --------------------------------------------------- 3. admission vs retire

async fn idle_harness() -> (
    Arc<faktor_core::time::TestClock>,
    std::sync::Arc<BrowserManager>,
    tempfile::TempDir,
) {
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
        max_pages_per_profile: 1,
        ..BrowserConfig::default()
    };
    let manager =
        BrowserManager::with_clock(supervisor, config, dir.path(), clock.clone()).unwrap();
    (clock, manager, dir)
}

async fn acquire_on(
    manager: &Arc<BrowserManager>,
    profile: &str,
) -> Result<faktor_browser::Page, BrowserError> {
    manager
        .acquire_page(
            "1688",
            &Harness::identity(profile),
            Harness::policy(),
            &PagePurpose::new("extraction"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn idle_retirement_refuses_an_admission_that_raced_it() {
    let (clock, manager, _dir) = idle_harness().await;
    let page = acquire_on(&manager, "p1").await.expect("setup page");
    let _ = page.close().await;
    clock.advance(1_500);

    let (seam, mut reached) = LifecycleSeam::new("p1");
    manager.set_lifecycle_seam(Some(seam.clone()));
    // The acquisition reaches instance_for, then pauses before admission.
    let acquire = {
        let manager = manager.clone();
        tokio::spawn(async move { acquire_on(&manager, "p1").await })
    };
    assert_eq!(reached.recv().await, Some("admission-begin"));
    // Retirement takes the lifecycle lock, verifies no permit is held and
    // pauses at its decision point (still holding the lock).
    let shutdown = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.shutdown_idle().await })
    };
    assert_eq!(reached.recv().await, Some("retire-decision"));
    // Release both: retirement flips to Retiring and removes the entry; the
    // acquisition then observes Retiring and refuses typed. It can never
    // start a target under a browser that is being closed.
    seam.grant(2);
    let stopped = shutdown.await.unwrap();
    assert_eq!(stopped, vec!["p1".to_string()]);
    let result = acquire.await.unwrap();
    assert_eq!(result.err().map(|e| e.code()), Some("retiring"));
    manager.set_lifecycle_seam(None);
    assert_eq!(manager.browser_count(), 0);
    let page = acquire_on(&manager, "p1").await.expect("retry relaunches");
    let _ = page.close().await;
    manager.shutdown_all().await;
}

#[tokio::test]
async fn idle_retirement_never_closes_a_browser_under_an_admitted_page() {
    let (clock, manager, _dir) = idle_harness().await;
    let page = acquire_on(&manager, "p1").await.expect("setup page");
    let _ = page.close().await;
    clock.advance(1_500);

    let (seam, mut reached) = LifecycleSeam::new("p1");
    manager.set_lifecycle_seam(Some(seam.clone()));
    // The acquisition holds its permit but has not created a target yet: the
    // exact window the old idle path closed the browser in.
    let acquire = {
        let manager = manager.clone();
        tokio::spawn(async move { acquire_on(&manager, "p1").await })
    };
    assert_eq!(reached.recv().await, Some("admission-begin"));
    seam.grant(1);
    assert_eq!(reached.recv().await, Some("admission-permit"));
    let stopped = manager.shutdown_idle().await;
    assert!(
        stopped.is_empty(),
        "a browser with an admitted (in-flight) page must not be retired"
    );
    assert_eq!(manager.browser_count(), 1, "the instance stays live");
    seam.grant(1);
    let page = acquire.await.unwrap().expect("admission wins");
    // The browser was genuinely alive underneath: the page works.
    let outcome = page
        .navigate(
            "https://first.test/page",
            deadline_in(10_000),
            &CancellationToken::new(),
        )
        .await
        .expect("the page must be usable");
    assert_eq!(outcome.lifecycle, faktor_browser::Lifecycle::Loaded);
    manager.set_lifecycle_seam(None);
    let _ = page.close().await;
    manager.shutdown_all().await;
}

#[tokio::test]
async fn page_admission_is_not_serialized_across_profiles() {
    let harness = Harness::new(json!({}), |config| {
        config.max_pages_per_profile = 1;
        config.max_browsers = 2;
    })
    .await;
    // Two live instances (the global admission mutex would serialize them).
    let first = harness.acquire("p1").await.expect("p1 setup");
    let _ = first.close().await;
    let second = harness.acquire("p2").await.expect("p2 setup");
    let _ = second.close().await;
    assert_eq!(harness.manager.browser_count(), 2);

    let (seam, mut reached) = LifecycleSeam::new("p1");
    harness.manager.set_lifecycle_seam(Some(seam.clone()));
    let p1 = {
        let manager = harness.manager.clone();
        tokio::spawn(async move { acquire_on(&manager, "p1").await })
    };
    assert_eq!(reached.recv().await, Some("admission-begin"));
    seam.grant(1);
    assert_eq!(reached.recv().await, Some("admission-permit"));
    // While p1 is paused mid-admission, p2 must complete: per-instance
    // admission, no global serialization.
    let p2 = tokio::time::timeout(Duration::from_secs(5), harness.acquire("p2"))
        .await
        .expect("p2 must not block on p1")
        .expect("p2 page");
    seam.grant(1);
    let p1 = p1.await.unwrap().expect("p1 page");
    harness.manager.set_lifecycle_seam(None);
    assert_eq!(
        harness.manager.page_count_for(&Harness::instance("p1")),
        Some(1)
    );
    assert_eq!(
        harness.manager.page_count_for(&Harness::instance("p2")),
        Some(1)
    );
    let _ = p1.close().await;
    let _ = p2.close().await;
    harness.manager.shutdown_all().await;
}

// ------------------------------------------------ 4. instance identity/modes

#[tokio::test]
async fn incognito_and_persistent_instances_are_distinct_and_visible() {
    let harness = Harness::new(json!({}), |config| {
        config.max_browsers = 2;
        config.max_pages_per_profile = 1;
    })
    .await;
    let identity = Harness::identity("p1");
    let persistent = harness.acquire("p1").await.expect("persistent page");
    let incognito = harness
        .manager
        .acquire_page(
            "1688",
            &identity,
            Harness::policy(),
            &PagePurpose::incognito("probe"),
            deadline_in(15_000),
            &CancellationToken::new(),
        )
        .await
        .expect("incognito page");
    let persistent_id = Harness::instance("p1");
    let incognito_id = Harness::incognito_instance("p1");

    // Both modes are visible to every lookup (the defect made incognito
    // invisible because lookups used the persistent key).
    assert_eq!(harness.manager.page_count_for(&persistent_id), Some(1));
    assert_eq!(harness.manager.page_count_for(&incognito_id), Some(1));
    assert!(harness.manager.is_live(&persistent_id));
    assert!(harness.manager.is_live(&incognito_id));
    assert_eq!(
        harness.manager.source_for(&incognito_id).as_deref(),
        Some("1688")
    );
    let mut ids = harness.manager.instances_for(&identity);
    ids.sort_by_key(|id| format!("{:?}", id.mode));
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&persistent_id));
    assert!(ids.contains(&incognito_id));

    // proxy_url_for is mode-qualified and multi-valued, never one arbitrary
    // browser.
    let proxies = harness.manager.proxy_url_for(&identity);
    assert_eq!(proxies.len(), 2);
    let modes: std::collections::BTreeSet<String> = proxies
        .iter()
        .map(|(id, _)| format!("{:?}", id.mode))
        .collect();
    assert_eq!(modes.len(), 2);
    assert_ne!(proxies[0].1, proxies[1].1, "two distinct brokers");

    // retire(identity) retires BOTH modes.
    let incognito_dir = launch_proxy_and_profile(&harness).1;
    harness.manager.retire(&identity).await.unwrap();
    assert_eq!(harness.manager.browser_count(), 0);
    assert_eq!(harness.manager.page_count_for(&persistent_id), None);
    assert_eq!(harness.manager.page_count_for(&incognito_id), None);
    assert!(
        !incognito_dir.exists(),
        "the incognito profile is removed on retire"
    );
    assert!(harness.browser_children().is_empty());
    let _ = persistent.close().await;
    let _ = incognito.close().await;
    assert_eq!(harness.manager.browser_count(), 0);
    // retire_instance on a specific id is exact.
    let page = harness.acquire("p1").await.expect("persistent again");
    let _ = page.close().await;
    harness
        .manager
        .retire_instance(&persistent_id)
        .await
        .unwrap();
    assert_eq!(harness.manager.browser_count(), 0);
    assert!(harness
        .manager
        .retire_instance(&persistent_id)
        .await
        .is_err());
    harness.manager.shutdown_all().await;
}

// -------------------------------------------------------- 5. downloads

fn timed_download_policy(max_bytes: u64) -> DownloadPolicy {
    DownloadPolicy {
        enabled: true,
        directory: Some("downloads".to_string()),
        max_bytes,
    }
}

async fn wait_for<F: FnMut() -> bool>(mut ready: F, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ready()
}

#[tokio::test]
async fn native_downloads_are_denied_centrally_and_progress_fails_closed() {
    let harness = Harness::new(
        json!({
            "browser_events": [
                {"method": "Browser.downloadWillBegin", "params": {
                    "guid": "g1",
                    "url": "https://first.test/file.pdf",
                    "suggestedFilename": "file.pdf",
                    "frameId": "f1"
                }},
                {"method": "Browser.downloadProgress", "params": {
                    "guid": "g1",
                    "receivedBytes": "not-a-number"
                }}
            ],
            "download_will_begin": {
                "guid": "g2",
                "url": "https://first.test/other.pdf",
                "suggestedFilename": "other.pdf"
            }
        }),
        |config| {
            config.downloads = timed_download_policy(1024);
        },
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
    // Native downloads are denied centrally (browser-domain events without a
    // session id): the profile's manager records and cancels both, and the
    // malformed progress field fails closed.
    let instance = Harness::instance("p1");
    assert!(
        wait_for(
            || harness
                .manager
                .download_stats_for(&instance)
                .unwrap()
                .blocked_total
                >= 2,
            Duration::from_secs(3)
        )
        .await,
        "both native downloads must be blocked centrally"
    );
    assert!(
        wait_for(
            || harness
                .manager
                .download_stats_for(&instance)
                .unwrap()
                .rejected_total
                >= 1,
            Duration::from_secs(3)
        )
        .await,
        "the malformed progress field must fail closed"
    );
    assert!(
        wait_for(
            || harness.journal_count("recv", "Browser.cancelDownload") >= 2,
            Duration::from_secs(3)
        )
        .await,
        "every native download is cancelled through CDP"
    );
    // Even with capture enabled, the native behavior installed is deny.
    let behavior = harness
        .journal_entries()
        .into_iter()
        .find(|entry| entry["method"] == "Browser.setDownloadBehavior")
        .expect("behavior installed");
    assert_eq!(behavior["params"]["behavior"], "deny");
    let records = harness.manager.download_records_for(&instance).unwrap();
    let malformed = records
        .iter()
        .find(|record| record.guid == "g1")
        .expect("the announced GUID is tracked centrally");
    assert_eq!(
        malformed.state,
        faktor_browser::DownloadState::Rejected,
        "the malformed progress field must reject the download"
    );
    let error = page.last_download_error().expect("typed download error");
    assert!(
        matches!(error.code(), "download_blocked" | "download_rejected"),
        "typed error: {error:?}"
    );
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn response_stage_capture_streams_under_the_cap_and_publishes_atomically() {
    let harness = Harness::new(
        json!({
            "pause_response": true,
            "stream_body_text": "captured-body",
            "stream_chunk": 4,
            "network": [
                {"request_id": "dl", "url": "https://first.test/report.bin",
                 "resource_type": "Document",
                 "content_disposition": "attachment; filename=\"report.bin\"",
                 "content_length": 13}
            ]
        }),
        |config| {
            config.downloads = timed_download_policy(4096);
            config.max_pages_per_profile = 1;
        },
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/download",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    let dest = harness
        .manager
        .profile_dir("p1")
        .unwrap()
        .join("downloads")
        .join("report.bin");
    assert!(
        wait_for(|| dest.exists(), Duration::from_secs(5)).await,
        "the capture must publish the complete file"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"captured-body");
    // No partial/temp file survives a successful publish.
    let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".faktor-download-")
        })
        .collect();
    assert!(leftovers.is_empty(), "no temp file may survive");
    let stats = harness
        .manager
        .download_stats_for(&Harness::instance("p1"))
        .unwrap();
    assert_eq!(stats.captured_total, 1);
    assert_eq!(stats.captured_bytes, 13);
    assert!(harness.journal_count("recv", "Fetch.takeResponseBodyAsStream") == 1);
    assert!(harness.journal_count("recv", "IO.read") >= 2);
    assert_eq!(harness.journal_count("recv", "IO.close"), 1);
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn over_bound_capture_aborts_immediately_and_publishes_nothing() {
    let harness = Harness::new(
        json!({
            "pause_response": true,
            "stream_body_bytes": 10_000,
            "stream_chunk": 512,
            "network": [
                {"request_id": "big", "url": "https://first.test/big.bin",
                 "resource_type": "Document",
                 "content_disposition": "attachment; filename=\"big.bin\""}
            ]
        }),
        |config| {
            config.downloads = timed_download_policy(1024);
        },
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    page.navigate(
        "https://first.test/download",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    let instance = Harness::instance("p1");
    assert!(
        wait_for(
            || harness
                .manager
                .download_stats_for(&instance)
                .unwrap()
                .cancelled_over_bound
                >= 1,
            Duration::from_secs(5)
        )
        .await,
        "the capture must abort at the cap"
    );
    // Abort is immediate: only the reads up to the exceeding chunk happen.
    let reads = harness.journal_count("recv", "IO.read");
    assert!(
        (2..=3).contains(&reads),
        "the stream must be abandoned immediately (reads={reads})"
    );
    assert_eq!(harness.journal_count("recv", "IO.close"), 1);
    let downloads = harness.manager.profile_dir("p1").unwrap().join("downloads");
    let entries: Vec<String> = std::fs::read_dir(&downloads)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        entries.is_empty(),
        "an over-bound capture must leave nothing behind: {entries:?}"
    );
    assert!(matches!(
        page.last_download_error(),
        Some(BrowserError::DownloadRejected { .. })
    ));
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}

#[tokio::test]
async fn declared_or_malformed_length_denies_before_streaming() {
    for (label, length_fields) in [
        ("over-bound", json!({"content_length": 5000})),
        ("malformed", json!({"content_length_raw": "bananas"})),
    ] {
        let harness = Harness::new(
            json!({
                "pause_response": true,
                "stream_body_text": "payload",
                "network": [
                    {"request_id": "dl", "url": "https://first.test/x.bin",
                     "resource_type": "Document",
                     "content_disposition": "attachment; filename=\"x.bin\""
                    }
                ]
            })
            .as_object()
            .cloned()
            .map(|mut object| {
                let item = object
                    .get_mut("network")
                    .and_then(Value::as_array_mut)
                    .and_then(|items| items.get_mut(0))
                    .and_then(Value::as_object_mut)
                    .unwrap();
                for (key, value) in length_fields.as_object().unwrap() {
                    item.insert(key.clone(), value.clone());
                }
                Value::Object(object)
            })
            .unwrap(),
            |config| {
                config.downloads = timed_download_policy(1024);
            },
        )
        .await;
        let page = harness.acquire("p1").await.expect("acquire");
        page.navigate(
            "https://first.test/download",
            deadline_in(10_000),
            &CancellationToken::new(),
        )
        .await
        .expect("navigate");
        let instance = Harness::instance("p1");
        assert!(
            wait_for(
                || harness
                    .manager
                    .download_stats_for(&instance)
                    .unwrap()
                    .rejected_total
                    >= 1,
                Duration::from_secs(5)
            )
            .await,
            "{label}: the response must be denied"
        );
        assert_eq!(
            harness.journal_count("recv", "Fetch.takeResponseBodyAsStream"),
            0,
            "{label}: no byte may be streamed"
        );
        assert!(
            harness.journal_count("recv", "Fetch.failRequest") >= 1,
            "{label}: the request must be failed"
        );
        let _ = page.close().await;
        harness.manager.shutdown_all().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_swapped_download_directory_cannot_publish_outside_the_profile() {
    let harness = Harness::new(
        json!({
            "pause_response": true,
            "stream_body_text": "exfil",
            "network": [
                {"request_id": "dl", "url": "https://first.test/x.bin",
                 "resource_type": "Document",
                 "content_disposition": "attachment; filename=\"x.bin\""}
            ]
        }),
        |config| {
            config.downloads = timed_download_policy(4096);
        },
    )
    .await;
    let page = harness.acquire("p1").await.expect("acquire");
    let downloads = harness.manager.profile_dir("p1").unwrap().join("downloads");
    assert!(
        downloads.is_dir(),
        "the download dir was prepared at launch"
    );
    // Swap the (already prepared) download directory for a symlink to a
    // directory outside the profile.
    let outside = harness._dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::remove_dir_all(&downloads).unwrap();
    std::os::unix::fs::symlink(&outside, &downloads).unwrap();
    page.navigate(
        "https://first.test/download",
        deadline_in(10_000),
        &CancellationToken::new(),
    )
    .await
    .expect("navigate");
    let instance = Harness::instance("p1");
    assert!(
        wait_for(
            || harness
                .manager
                .download_stats_for(&instance)
                .unwrap()
                .rejected_total
                >= 1,
            Duration::from_secs(5)
        )
        .await,
        "the swapped download directory must fail the capture"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing may ever be written through the symlink"
    );
    let _ = page.close().await;
    harness.manager.shutdown_all().await;
}
