//! `main_worker_tests`: out-of-line slice of the CLI test module.

use super::*;

use faktor_core::model::ModelCapabilities;

use faktor_core::CancellationToken;

use faktor_core::OpId;

use faktor_provider::testing::{sse_body, MockAction, MockServer};

use faktor_provider::{
    ContentPart, GenericAgentRequest, ProviderChunk, ProviderError, RequestMessage, RequestMeta,
    Role, ToolSpec,
};

use futures::StreamExt;

use std::pin::Pin;

/// Permission requester that never blocks on a UI (text-only turns never
/// ask, but AgentDeps requires one deterministically).
pub(crate) struct AlwaysAllow;

impl faktor_agent::PermissionRequester for AlwaysAllow {
    fn request(
        &self,
        _session: SessionId,
        _permission: &faktor_session::PermissionRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = faktor_core::Result<faktor_core::capability::PermissionDecision>,
                > + Send,
        >,
    > {
        Box::pin(async { Ok(faktor_core::capability::PermissionDecision::Allow) })
    }
}

/// Minimal REAL daemon AgentDeps over an open session manager: text-only
/// turns, no MCP/verifier/supervisor (nothing here ever runs a process).
pub(crate) fn test_agent(
    session: Arc<SessionManager>,
    registry: ProviderRegistry,
) -> Arc<AgentRuntime> {
    let cas = session.cas();
    let deps = AgentDeps {
        session: session.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(faktor_agent::NoEvidence),
        tools: Arc::new(ToolRegistry::new()),
        cas: Some(cas),
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        verification: faktor_agent::VerificationService::disabled(),
        hooks: None,
        instructions_resolver: daemon_instructions_resolver(&session),
        // Test graph: the passthrough pin (session-configured
        // provider/model win) + the REAL durable ledger over this
        // session manager (reservations ride the tempdir store).
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
        model: "default".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are Faktor.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 2000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    };
    AgentRuntime::new(deps).unwrap()
}

/// A chat model that answers every stream identically (repeated evidence
/// turns must never run out of script).
pub(crate) struct AlwaysOk;

impl Provider for AlwaysOk {
    fn id(&self) -> &str {
        "fake"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities {
            tools: true,
            ..Default::default()
        }
    }

    fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderChunk::Text { text: "ok".into() }),
            Ok(ProviderChunk::Done),
        ]))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn backup_blocking_work_never_occupies_the_single_tokio_worker() {
    // P0-46: the sync snapshot+rotation is executed via spawn_blocking.
    // With ONE Tokio worker, a probe task spawned WHILE the snapshot is
    // in flight must complete promptly — if the SQLite backup ran
    // inline on the worker, nothing else could run until the whole
    // snapshot finished (worker starvation).
    let dir = tempfile::tempdir().unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    // Seed a session + message so a recursive insert can build a
    // multi-hundred-thousand-row part table (a snapshot that takes real
    // time; the probe needs an observable overlap window).
    let ws = session.create_workspace("/w").unwrap();
    let sid = session.create_session(ws, "seed", "p", "m").unwrap().id();
    let mid = session
        .store()
        .put_message(sid, 1, "user", serde_json::json!({"text": "x"}))
        .unwrap();
    let store = session.store();
    for _ in 0..2 {
        store
                .sql_execute(&format!(
                    "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 300000)
                     INSERT INTO part(message_id, kind, data, created_ms)
                     SELECT {mid}, 'text', '{{\"text\":\"padding\"}}', 1 FROM cnt;"
                ))
                .unwrap();
    }
    // An already-due gate (no backups exist yet): the task sleeps the
    // post-ready delay, then snapshots on the blocking pool.
    let backup = spawn_startup_backup(store, dir.path().to_path_buf());
    // Wait (bounded) until the blocking snapshot observably started: the
    // in-progress `.db.tmp-*` file exists only while backup_to runs.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let started = std::fs::read_dir(dir.path().join("backups"))
            .map(|rd| {
                rd.flatten()
                    .any(|f| f.file_name().to_string_lossy().contains(".db.tmp-"))
            })
            .unwrap_or(false);
        if started {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the blocking snapshot never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    // The snapshot is mid-flight on the blocking pool: a probe on the
    // SINGLE worker must run in well under the snapshot's duration.
    let probe = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        tokio::task::spawn(async { 42 }),
    )
    .await
    .expect("the worker must stay responsive while the backup blocks")
    .expect("probe task panicked");
    assert_eq!(probe, 42);
    // And the backup finishes with exactly one complete snapshot.
    tokio::time::timeout(std::time::Duration::from_secs(60), backup)
        .await
        .expect("the backup task must finish")
        .expect("backup task panicked");
    assert_eq!(list_backups(dir.path()).len(), 1, "one complete snapshot");
}

/// The additive `doctor --config` worker-plane boundary audit: the
/// resolved exposure decision (with the trusted-gateway acknowledgement)
/// is reported, and a refused boundary is an ISSUE — never repaired.
#[test]
fn doctor_reports_the_worker_plane_boundary_and_acknowledgement() {
    let dir = tempfile::tempdir().unwrap();
    {
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        session
            .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
            .unwrap();
    }
    // No --config: no worker-plane line at all (config-free doctor
    // unchanged).
    let report = doctor_run(dir.path(), false);
    assert!(!report
        .lines
        .iter()
        .any(|line| line.starts_with("worker plane:")));
    assert_eq!(report.issues, 0, "{:?}", report.lines);

    let config = dir.path().join("config.json");
    // A disabled section is an honest line (with its explicit state), not
    // an issue.
    std::fs::write(&config, r#"{"model": "m"}"#).unwrap();
    let report = doctor_run_with_config(dir.path(), false, Some(&config));
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.contains("worker plane: disabled")
                && line.contains("state=disabled")
                && line.contains("enabled=false")),
        "{:?}",
        report.lines
    );
    assert_eq!(report.issues, 0, "{:?}", report.lines);

    // An enabled plane renders enabled/bind/state (the default loopback
    // bind and the worker-token auth mode are named).
    std::fs::write(
            &config,
            r#"{"model": "m", "cloud": {"enabled": true}, "workers": {"enabled": true, "organization": "org_local"}, "worker_plane": {"enabled": true}}"#,
        )
        .unwrap();
    let report = doctor_run_with_config(dir.path(), false, Some(&config));
    let enabled_line = report
        .lines
        .iter()
        .find(|line| line.starts_with("worker plane: enabled"))
        .expect("an enabled section renders its state");
    assert!(
        enabled_line.contains("state=enabled")
            && enabled_line.contains("enabled=true")
            && enabled_line.contains("bind=127.0.0.1:8790")
            && enabled_line.contains("auth=worker_tokens"),
        "{enabled_line}"
    );
    assert_eq!(report.issues, 0, "{:?}", report.lines);

    // A refused boundary is an issue naming the typed refusal code and
    // its refused state.
    std::fs::write(
            &config,
            r#"{"model": "m", "cloud": {"enabled": true}, "workers": {"enabled": true, "organization": "org_local"}, "worker_plane": {"enabled": true, "bind": "0.0.0.0:8790"}}"#,
        )
        .unwrap();
    let report = doctor_run_with_config(dir.path(), false, Some(&config));
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.contains("worker plane: FAILED")
                && line.contains("state=refused")
                && line.contains("enabled=true")
                && line.contains("worker_plane_boundary_refused")
                && line.contains("worker-plane deployment boundary")),
        "{:?}",
        report.lines
    );
    assert!(report.issues >= 1);

    // Gateway mode: the acknowledgement is recorded (and binds nothing).
    std::fs::write(
            &config,
            r#"{"model": "m", "cloud": {"enabled": true}, "workers": {"enabled": true, "organization": "org_local"}, "worker_plane": {"enabled": true, "bind": "0.0.0.0:8790", "trusted_gateway": true}}"#,
        )
        .unwrap();
    let report = doctor_run_with_config(dir.path(), false, Some(&config));
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.contains("worker plane: trusted_gateway acknowledgement recorded")),
        "{:?}",
        report.lines
    );
    assert!(
        report
            .lines
            .iter()
            .any(|line| line.contains("exposure=beyond-loopback")
                && line.contains("trusted_gateway=true")),
        "{:?}",
        report.lines
    );
    assert_eq!(report.issues, 0, "{:?}", report.lines);
}

/// A legacy commercial database with no policy marker is flagged (its
/// last writer did not acknowledge with `synchronous = FULL`).
#[test]
fn doctor_flags_a_cloud_family_db_without_the_policy_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control-plane.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE legacy (id TEXT PRIMARY KEY); PRAGMA journal_mode = WAL;")
            .unwrap();
    }
    let report = doctor_run(dir.path(), false);
    assert!(
        report
            .lines
            .iter()
            .any(|l| l.contains("durability policy marker missing")),
        "{:?}",
        report.lines
    );
    assert!(report.issues >= 1, "{:?}", report.lines);
}

// ------------------------------------------------------------- wiring

/// One chat request whose user text can echo configured secrets.
pub(crate) fn chat_req(text: &str) -> GenericAgentRequest {
    GenericAgentRequest {
        model: "m".into(),
        system: "sys".into(),
        messages: vec![RequestMessage {
            role: Role::User,
            content: vec![ContentPart::text(text)],
        }],
        tools: vec![ToolSpec {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        max_output: Some(64),
        reasoning: None,
        stream: true,
        meta: RequestMeta {
            operation_id: OpId::new(1),
            session_id: SessionId::new(1),
            provider: "cli-wiring-test".into(),
            attempt: 0,
            deadline_ms: 10_000,
            cancellation: CancellationToken::new(),
        },
    }
}

/// Drive one chat call to completion; `Err` carries the provider error
/// (a policy/secret refusal arrives before any server contact).
pub(crate) async fn chat_text(
    provider: Arc<dyn Provider>,
    text: &str,
) -> Result<String, ProviderError> {
    let mut stream = provider.stream(chat_req(text));
    let mut out = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(ProviderChunk::Text { text: t }) => out.push_str(&t),
            Ok(ProviderChunk::Done) => break,
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// A config with ONE OpenAI-compatible provider at `base` and the given
/// sandbox network rows (`None` = no sandbox section = crate defaults).
pub(crate) fn egress_cfg(
    dir: &std::path::Path,
    file: &str,
    base: &str,
    key_env: Option<&str>,
    rows: Option<&[String]>,
) -> config::Config {
    let mut body = serde_json::json!({
        "model": "m",
        "providers": [{
            "kind": "open_ai",
            "id": "mocked",
            "base_url": base,
            "api_key_env": key_env,
            // The tests below target loopback mock servers, so the
            // entry carries the explicit loopback address-class rule.
            "allow_loopback": true,
        }],
    });
    if let Some(rows) = rows {
        body["sandbox"] = serde_json::json!({ "network": rows });
    }
    let path = dir.join(file);
    std::fs::write(&path, serde_json::to_string(&body).unwrap()).unwrap();
    config::Config::load(&path).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_provider_transports_carry_the_destination_policy() {
    let server = MockServer::new();
    server.route(
        "POST",
        "/chat/completions",
        MockAction::Respond {
            status: 200,
            body: sse_body(&[serde_json::json!({
                "choices": [{"delta": {"content": "allowed"}, "finish_reason": "stop"}]
            })]),
        },
    );
    let base = server.base_url().await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let allow_row = format!("http://127.0.0.1:{port}");

    // (a) The allow row rides into the CONSTRUCTED transports: the chat
    // request reaches the mock and streams.
    let dir = tempfile::tempdir().unwrap();
    let cfg = egress_cfg(
        dir.path(),
        "allow.json",
        &base,
        None,
        Some(std::slice::from_ref(&allow_row)),
    );
    let graph = build_daemon(dir.path(), Some(cfg)).unwrap();
    let provider = graph.providers.get("mocked").expect("provider registered");
    let text = chat_text(provider, "hello").await.expect("allowed chat");
    assert_eq!(text, "allowed");
    assert_eq!(server.request_count(), 1, "the allowed request arrived");
    drop(graph);

    // (b) Rows that deny the actual host (only a DIFFERENT port is
    // allowlisted): the SAME call fails pre-connect with the typed
    // denial and is never retried — the server sees nothing new.
    let dir2 = tempfile::tempdir().unwrap();
    let wrong_row = format!("http://127.0.0.1:{}", port.wrapping_add(1));
    let cfg = egress_cfg(dir2.path(), "deny.json", &base, None, Some(&[wrong_row]));
    let graph = build_daemon(dir2.path(), Some(cfg)).unwrap();
    let err = chat_text(graph.providers.get("mocked").unwrap(), "hello")
        .await
        .expect_err("a denied destination must fail the chat");
    assert!(err.message.contains("denied"), "{}", err.message);
    assert!(!err.retryable, "policy denials are never retried");
    assert_eq!(server.request_count(), 1, "deny happened before connect");
    drop(graph);

    // (c) An empty row list denies EVERY destination before connect.
    let dir3 = tempfile::tempdir().unwrap();
    let cfg = egress_cfg(dir3.path(), "denyall.json", &base, None, Some(&[]));
    let graph = build_daemon(dir3.path(), Some(cfg)).unwrap();
    let err = chat_text(graph.providers.get("mocked").unwrap(), "hello")
        .await
        .expect_err("an empty allowlist denies everything");
    assert!(err.message.contains("denied"), "{}", err.message);
    assert_eq!(server.request_count(), 1);
    drop(graph);

    // (d) The DEFAULT daemon (no sandbox section) enforces the sandbox
    // crate's frozen provider-endpoint allowlist: the localhost mock is
    // NOT on it, so egress is denied pre-connect — adapters are never
    // permissively default-transported.
    let dir4 = tempfile::tempdir().unwrap();
    let cfg = egress_cfg(dir4.path(), "default.json", &base, None, None);
    let graph = build_daemon(dir4.path(), Some(cfg)).unwrap();
    let err = chat_text(graph.providers.get("mocked").unwrap(), "hello")
        .await
        .expect_err("the frozen default allowlist denies the mock host");
    assert!(err.message.contains("denied"), "{}", err.message);
    assert_eq!(server.request_count(), 1, "default policy: no connect");
    drop(graph);
}

/// Cloud-disabled parity: a daemon with `[cloud] enabled = false` (and
/// with an absent section) creates NO control-plane/SCM database and
/// keeps the frozen startup line; an enabled daemon opens exactly those
/// two databases under its data dir.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloud_disabled_daemon_creates_no_cloud_databases_and_enabled_opens_them() {
    for (label, config_json, expect_cloud) in [
        ("absent", r#"{"model": "m"}"#, false),
        (
            "disabled",
            r#"{"model": "m", "cloud": {"enabled": false}}"#,
            false,
        ),
        (
            "enabled",
            r#"{"model": "m", "cloud": {"enabled": true, "database": "cp.db", "scm_database": "repos.db"}}"#,
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("faktor-plus.json");
        std::fs::write(&config, config_json).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        let control_plane_db = dir.path().join("cp.db");
        let scm_db = dir.path().join("repos.db");
        let default_control_plane_db = dir.path().join("control-plane.db");
        let default_scm_db = dir.path().join("scm.db");
        if expect_cloud {
            assert!(
                control_plane_db.exists() && scm_db.exists(),
                "{label}: an enabled [cloud] section must open its databases"
            );
        } else {
            assert!(
                !control_plane_db.exists()
                    && !scm_db.exists()
                    && !default_control_plane_db.exists()
                    && !default_scm_db.exists(),
                "{label}: a disabled [cloud] section must create no database"
            );
        }
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
            .await
            .expect("daemon must stop on shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }
}

/// GitHub App end-to-end: a daemon configured with a MOCK GitHub base
/// URL builds the real adapter/token source/inbox/sync from the
/// operator-staged payloads, runs the bounded initial sync after
/// readiness (durable rows under the data dir), and dispatches a signed
/// webhook delivery into the wired inbox WITHOUT the daemon password —
/// which triggers one idempotent re-sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn github_app_daemon_syncs_durably_and_dispatches_signed_webhooks() {
    use faktor_scm::ScmStore;
    const WEBHOOK_SECRET: &[u8] = b"hook-secret";
    let mock = MockServer::new();
    let (mock_addr, _mock_task) = mock.clone().serve().await;
    let base = format!("http://{mock_addr}");
    mock.route(
        "GET",
        "/app/installations",
        MockAction::Respond {
            status: 200,
            body: serde_json::json!([{
                "id": 7,
                "account": {"login": "acme", "type": "Organization"},
                "permissions": {"contents": "write"},
            }])
            .to_string(),
        },
    );
    mock.route(
        "POST",
        "/app/installations/7/access_tokens",
        MockAction::Respond {
            status: 200,
            body: serde_json::json!({
                "token": "ghs_test",
                "expires_at": "2099-01-01T00:00:00Z",
                "permissions": {
                    "contents": "write",
                    "pull_requests": "write",
                    "issues": "write",
                    "metadata": "read",
                },
            })
            .to_string(),
        },
    );
    let repositories = serde_json::json!({
        "total_count": 1,
        "repositories": [{
            "id": 11,
            "name": "widgets",
            "full_name": "acme/widgets",
            "default_branch": "main",
            "private": true,
            "archived": false,
            "html_url": "http://example.test/acme/widgets",
            "owner": {"login": "acme"},
        }],
    })
    .to_string();
    mock.route(
        "GET",
        "/installation/repositories",
        MockAction::Sequence {
            actions: vec![
                MockAction::Respond {
                    status: 200,
                    body: repositories.clone(),
                },
                MockAction::Respond {
                    status: 200,
                    body: repositories,
                },
            ],
        },
    );

    let dir = tempfile::tempdir().unwrap();
    // Operator-staged payloads (0600 on unix).
    let payloads = dir.path().join("payloads");
    std::fs::create_dir_all(&payloads).unwrap();
    for (name, body) in [
        ("app.pem", crate::scm_daemon::TEST_PRIVATE_KEY.as_bytes()),
        ("hook.secret", WEBHOOK_SECRET),
    ] {
        let path = payloads.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let config = dir.path().join("faktor-plus.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "model": "m",
            "cloud": {
                "enabled": true,
                "database": "cp.db",
                "scm_database": "repos.db",
                "github_app": {
                    "enabled": true,
                    "app_id": 12345,
                    "private_key": "app.pem",
                    "webhook_secret": "hook.secret",
                    "api_base": base,
                    "organization": "org_acme",
                },
            },
            "sandbox": {"network": [base]},
        })
        .to_string(),
    )
    .unwrap();
    // A concrete loopback port for the daemon (the webhook route needs a
    // known address; the daemon never needs the mock's policy to reach
    // ITSELF).
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let dir2 = dir.path().to_path_buf();
    let daemon = tokio::task::spawn(async move {
        serve_impl(port, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");

    // The initial sync runs post-readiness; poll the DURABLE rows.
    let scm_path = dir.path().join("repos.db");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let store = faktor_scm::SqliteScmStore::open(&scm_path).unwrap();
        if !store
            .repositories_for_organization("org_acme", 0, 10)
            .unwrap()
            .is_empty()
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the initial sync never landed durable rows"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // A signed delivery (NO daemon password) is dispatched into the
    // wired inbox and schedules one idempotent re-sync.
    let client = reqwest::Client::new();
    let body = br#"{"installation":{"id":7},"action":"created"}"#;
    let webhook = client
        .post(format!("http://127.0.0.1:{port}/native/scm/webhook"))
        .header("x-github-delivery", "delivery-1")
        .header("x-github-event", "installation")
        .header(
            "x-hub-signature-256",
            format!(
                "sha256={}",
                faktor_scm::hmac_sha256_hex(WEBHOOK_SECRET, body)
            ),
        )
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(webhook.status(), 200);
    let webhook: serde_json::Value = webhook.json().await.unwrap();
    assert_eq!(webhook["status"], "accepted");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let repo_calls = mock
            .requests()
            .iter()
            .filter(|(method, path, _)| method == "GET" && path == "/installation/repositories")
            .count();
        if repo_calls >= 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the webhook re-sync never ran"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let store = faktor_scm::SqliteScmStore::open(&scm_path).unwrap();
    assert_eq!(
        store
            .repositories_for_organization("org_acme", 0, 10)
            .unwrap()
            .len(),
        1,
        "the re-sync upserts instead of duplicating"
    );
    assert_eq!(store.webhook_deliveries(10).unwrap().len(), 1);
    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
        .await
        .expect("daemon must stop on shutdown")
        .expect("serve_impl returns Ok")
        .unwrap();
}

/// P1 webhook arithmetic over the REAL HTTP route: attacker-controlled
/// timestamps (`i64::MIN`, `i64::MAX`, far-future, stale, zero, and
/// malformed strings) are typed 401 refusals — never a panic, never a
/// 500 — and a recent timestamp with a good signature is accepted. The
/// old `(now_ms - ts).abs()` panicked (debug) / wrapped (release) on
/// `i64::MIN` before any signature check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webhook_timestamp_arithmetic_is_fail_closed_over_http() {
    use faktor_scm::ScmStore;
    const WEBHOOK_SECRET: &[u8] = b"hook-secret";
    let mock = MockServer::new();
    let (mock_addr, _mock_task) = mock.clone().serve().await;
    let base = format!("http://{mock_addr}");

    let dir = tempfile::tempdir().unwrap();
    let payloads = dir.path().join("payloads");
    std::fs::create_dir_all(&payloads).unwrap();
    for (name, body) in [
        ("app.pem", crate::scm_daemon::TEST_PRIVATE_KEY.as_bytes()),
        ("hook.secret", WEBHOOK_SECRET),
    ] {
        let path = payloads.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let config = dir.path().join("faktor-plus.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "model": "m",
            "cloud": {
                "enabled": true,
                "database": "cp.db",
                "scm_database": "repos.db",
                "github_app": {
                    "enabled": true,
                    "app_id": 12345,
                    "private_key": "app.pem",
                    "webhook_secret": "hook.secret",
                    "api_base": base,
                    "organization": "org_acme",
                },
            },
            "sandbox": {"network": [base]},
        })
        .to_string(),
    )
    .unwrap();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let dir2 = dir.path().to_path_buf();
    let daemon = tokio::task::spawn(async move {
        serve_impl(port, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let body = br#"{"installation":{"id":7},"action":"created"}"#;
    let client = reqwest::Client::new();
    let post = |delivery: &str, timestamp: Option<&str>, signed_input: &[u8]| {
        let signature = format!(
            "sha256={}",
            faktor_scm::hmac_sha256_hex(WEBHOOK_SECRET, signed_input)
        );
        let mut request = client
            .post(format!("http://127.0.0.1:{port}/native/scm/webhook"))
            .header("x-github-delivery", delivery)
            .header("x-github-event", "installation")
            .header("x-hub-signature-256", signature);
        if let Some(ts) = timestamp {
            request = request.header("x-faktor-timestamp", ts);
        }
        request.body(body.to_vec())
    };

    // Every hostile timestamp is signed over `{ts}.{body}` (a VALID
    // signature for that timestamp), so the only reason to refuse is the
    // replay-window arithmetic.
    let signed_over = |ts: &str| {
        let mut input = format!("{ts}.").into_bytes();
        input.extend_from_slice(body);
        input
    };
    let hostile: Vec<(&str, String)> = vec![
        ("i64::MIN", i64::MIN.to_string()),
        ("i64::MAX", i64::MAX.to_string()),
        ("far-future", (now_ms + 3_600_000).to_string()),
        ("stale", (now_ms - 3_600_000).to_string()),
        ("zero", "0".to_string()),
        ("malformed", "not-a-number".to_string()),
        ("overflowing-number", "99999999999999999999999".to_string()),
    ];
    for (label, ts) in &hostile {
        let response = post(&format!("d-hostile-{label}"), Some(ts), &signed_over(ts))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{label}: the route must answer, not reset: {e}"));
        assert_eq!(
            response.status(),
            401,
            "{label}: hostile timestamp must be a typed refusal"
        );
        let refusal: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            refusal["error"]["code"], "scm_webhook_unauthorized",
            "{label}: {refusal}"
        );
    }
    // A malformed timestamp + a VALID body-only signature (the GitHub
    // mode shape) must still refuse: the malformed header may not select
    // a weaker verification mode.
    let response = post("d-malformed-body-sig", Some("not-a-number"), body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let refusal: serde_json::Value = response.json().await.unwrap();
    assert_eq!(refusal["error"]["code"], "scm_webhook_unauthorized");

    // A recent timestamp with a good signature over `{now}.{body}` is
    // accepted (and only that one delivery is durably claimed).
    let now = now_ms.to_string();
    let accepted = post("d-accepted", Some(&now), &signed_over(&now))
        .send()
        .await
        .unwrap();
    assert_eq!(
        accepted.status(),
        200,
        "a fresh signed delivery is accepted"
    );
    let accepted: serde_json::Value = accepted.json().await.unwrap();
    assert_eq!(accepted["status"], "accepted", "{accepted}");

    let store = faktor_scm::SqliteScmStore::open(&dir.path().join("repos.db")).unwrap();
    assert_eq!(
        store.webhook_deliveries(10).unwrap().len(),
        1,
        "no hostile delivery may reach the durable inbox"
    );
    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
        .await
        .expect("daemon must stop on shutdown")
        .expect("serve_impl returns Ok")
        .unwrap();
}

/// Updater-disabled parity: a daemon with `[updater] enabled = false`
/// (and with an absent section) creates NO `update.db` and NO install
/// directory; an enabled daemon creates both under its data dir. The
/// operator key is a real ed25519 public key (the section validates key
/// material at load time).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn updater_disabled_daemon_creates_nothing_and_enabled_opens_the_store() {
    const KEY: &str = "PMIf08ao62O4xMR4upvk5ymt++8EcWRtWHWZGLa4TKo=";
    let enabled_json = format!(
        r#"{{"model": "m", "updater": {{"enabled": true, "channel": "beta", "keys": [{{"id": "op-test", "public_key": "{KEY}"}}]}}}}"#
    );
    for (label, config_json, expect_updater) in [
        ("absent", r#"{"model": "m"}"#.to_string(), false),
        (
            "disabled",
            r#"{"model": "m", "updater": {"enabled": false}}"#.to_string(),
            false,
        ),
        ("enabled", enabled_json, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("faktor-plus.json");
        std::fs::write(&config, &config_json).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        let update_db = dir.path().join("update.db");
        let install_root = dir.path().join("install");
        if expect_updater {
            assert!(
                update_db.exists(),
                "{label}: an enabled [updater] section must open its store"
            );
            assert!(
                install_root.join("artifacts").is_dir() && install_root.join("staging").is_dir(),
                "{label}: an enabled [updater] section must create the install layout"
            );
            assert!(
                !install_root.join("current").exists(),
                "{label}: no artifact is installed before an apply"
            );
        } else {
            assert!(
                !update_db.exists(),
                "{label}: a disabled [updater] section must create no database"
            );
            assert!(
                !install_root.exists(),
                "{label}: a disabled [updater] section must create no install root"
            );
        }
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
            .await
            .expect("daemon must stop on shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }
}

/// Billing-disabled parity: a daemon with `[billing] enabled = false`
/// (and with an absent section) creates NO `billing.db`; an enabled
/// section (which requires `[cloud]`, the tenant principal) opens the
/// durable usage/credit ledger under the data dir and provisions the
/// configured account idempotently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn billing_disabled_daemon_creates_no_billing_db_and_enabled_provisions_it() {
    let enabled_json = r#"{
            "model": "m",
            "cloud": {"enabled": true, "database": "cp.db", "scm_database": "repos.db"},
            "billing": {
                "enabled": true,
                "database": "metering.db",
                "organization": "org_local",
                "account": "acct_local",
                "managed_providers": ["managed-provider"],
                "default_plan": "pro",
                "plans": {"pro": {"plan_id": "pro", "limits": {"max_active_tasks": 1}}}
            }
        }"#;
    for (label, config_json, expect_billing) in [
        ("absent", r#"{"model": "m"}"#.to_string(), false),
        (
            "disabled",
            r#"{"model": "m", "billing": {"enabled": false}}"#.to_string(),
            false,
        ),
        ("enabled", enabled_json.to_string(), true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("faktor-plus.json");
        std::fs::write(&config, &config_json).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        let billing_db = dir.path().join("metering.db");
        let default_billing_db = dir.path().join("billing.db");
        if expect_billing {
            assert!(
                billing_db.exists(),
                "{label}: an enabled [billing] section must open its ledger"
            );
            // The provisioned account is durable in THAT file: the
            // wiring created exactly one organization-scoped account.
            let store = std::sync::Arc::new(
                faktor_cloud::SqliteControlPlaneStore::open(&billing_db).unwrap(),
            ) as std::sync::Arc<dyn faktor_cloud::BillingStore>;
            let organization = faktor_cloud::OrganizationId::try_new("org_local").unwrap();
            let accounts = store.billing_accounts(&organization, None, 10).unwrap();
            assert_eq!(accounts.len(), 1, "{label}: exactly one account");
            assert_eq!(accounts[0].id.as_str(), "acct_local");
            assert!(accounts[0].managed, "the default account is managed");
            assert!(
                store
                    .billing_accounts(
                        &faktor_cloud::OrganizationId::try_new("org_foreign").unwrap(),
                        None,
                        10
                    )
                    .unwrap()
                    .is_empty(),
                "{label}: a foreign organization owns no account"
            );
        } else {
            assert!(
                !billing_db.exists() && !default_billing_db.exists(),
                "{label}: a disabled [billing] section must create no database"
            );
        }
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
            .await
            .expect("daemon must stop on shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }
}

/// Worker-plane parity: an absent or disabled `[workers]` section
/// creates NO worker database (the daemon is byte-identical to the
/// pre-worker-plane daemon); an enabled section (which requires
/// `[cloud]`) opens the durable plane with its OWN migration ladder and
/// wires the placement seam into the daemon's TaskExecutor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workers_disabled_daemon_creates_no_workers_db_and_enabled_opens_the_plane() {
    let enabled_json = r#"{
            "model": "m",
            "cloud": {"enabled": true, "database": "cp.db", "scm_database": "repos.db"},
            "workers": {
                "enabled": true,
                "database": "wp.db",
                "organization": "org_local",
                "trust_domain": "org_local",
                "toolchains": ["rust"]
            }
        }"#;
    for (label, config_json, expect_workers) in [
        ("absent", r#"{"model": "m"}"#.to_string(), false),
        (
            "disabled",
            r#"{"model": "m", "workers": {"enabled": false}}"#.to_string(),
            false,
        ),
        ("enabled", enabled_json.to_string(), true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("faktor-plus.json");
        std::fs::write(&config, &config_json).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        let workers_db = dir.path().join("wp.db");
        let default_workers_db = dir.path().join("workers.db");
        if expect_workers {
            assert!(
                workers_db.exists(),
                "{label}: an enabled [workers] section must open its durable plane"
            );
            // The plane's OWN migration ladder is applied (v2: the v1
            // schema + the durability policy marker) — the commercial
            // control-plane user_version is untouched.
            let store = faktor_worker::SqliteWorkerStore::open(&workers_db).unwrap();
            assert_eq!(faktor_worker::schema_version(&store).unwrap(), 2);
            let plane = faktor_worker::WorkerPlane::with_system_clock(std::sync::Arc::new(store));
            let organization = faktor_cloud::OrganizationId::try_new("org_local").unwrap();
            assert!(plane
                .list_workers(&organization, None, 10)
                .unwrap()
                .items
                .is_empty());
        } else {
            assert!(
                !workers_db.exists() && !default_workers_db.exists(),
                "{label}: a disabled [workers] section must create no database"
            );
        }
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(60), daemon)
            .await
            .expect("daemon shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }
}

/// The `[worker_plane]` serve wiring: the enabled second listener really
/// serves (the worker route answers on its own socket), and the daemon's
/// shutdown sequence JOINS the owned serve task within the bound — the
/// socket is released after `serve_impl` returns Ok, never detached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_plane_listener_is_owned_and_joined_on_daemon_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    // A concrete loopback port for the worker plane (the native listener
    // prints its own address on the startup line, the worker plane does
    // not); the probe is dropped so the daemon can bind it.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let worker_port = probe.local_addr().unwrap().port();
    drop(probe);
    let config = dir.path().join("faktor-plus.json");
    std::fs::write(
        &config,
        format!(
            r#"{{
                    "model": "m",
                    "cloud": {{"enabled": true, "database": "cp.db", "scm_database": "repos.db"}},
                    "workers": {{
                        "enabled": true,
                        "database": "wp.db",
                        "organization": "org_local",
                        "trust_domain": "org_local"
                    }},
                    "worker_plane": {{"enabled": true, "bind": "127.0.0.1:{worker_port}"}}
                }}"#
        ),
    )
    .unwrap();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let dir2 = dir.path().to_path_buf();
    let daemon = tokio::task::spawn(async move {
        serve_impl(0, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
        .await
        .expect("serve must reach the startup line")
        .expect("ready signal");
    // The worker plane is live on its OWN socket (a malformed body is a
    // strict-DTO 400, not a connection refusal).
    let response = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{worker_port}/native/workers/register"
        ))
        .json(&serde_json::json!({ "hostile": true }))
        .send()
        .await
        .expect("the worker-plane listener must answer");
    assert_eq!(response.status(), 400);
    // Shutdown joins the owned serve task; the daemon returns Ok.
    let _ = shutdown_tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(60), daemon)
        .await
        .expect("daemon shutdown (worker plane joined)")
        .expect("serve_impl returns Ok")
        .unwrap();
    // The owner completed: the worker-plane listener socket is released.
    let rebind = std::net::TcpListener::bind(("127.0.0.1", worker_port))
        .expect("the worker-plane socket must be released after shutdown");
    drop(rebind);
}

/// The daemon shutdown sequence stops the graph-hosted repository index
/// reconciliation worker: with an ACTIVE worker the sequence cancels and
/// JOINS it within its bound (no ghost passes left behind — the owner
/// reports no task left to join afterwards), and with a worker that was
/// never started the sequence is a safe, bounded no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_joins_the_graph_index_worker_bounded() {
    use faktor_index::{IndexService, WorkerShutdown, WorkerState};

    /// Bounded wait for the owned worker to reach a terminal state.
    async fn wait_state(index: &Arc<IndexService>, want: WorkerState) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while index.worker_status().state != want {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the index worker never reached {want:?}: {:?}",
                index.worker_status()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// One bounded shutdown-sequence run over the shared executor.
    async fn run_shutdown(
        tasks: &Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
        index: &Arc<IndexService>,
    ) {
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            shutdown_serving_daemon(
                tasks,
                None,
                None,
                Some(index.clone()),
                None,
                tokio::spawn(std::future::pending::<()>()),
                None,
                None,
                tokio::spawn(async {}),
            ),
        )
        .await
        .expect("the shutdown sequence must stay bounded");
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("owner");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("lib.rs"), b"pub fn indexed() -> i64 { 1 }\n").unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
    let index = IndexService::open(
        session.store(),
        dir.path().join("index_data"),
        faktor_fs::WorkspaceFileService::new(),
    )
    .unwrap();

    // The drive-drain authority the exact serve shutdown sequence uses
    // (the same construction `build_daemon` performs).
    let agent = test_agent(session.clone(), ProviderRegistry::new());
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
    let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
        session.clone(),
        dir.path().join("shadows"),
    )
    .unwrap();
    let tasks = Arc::new(
        faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            session.clone(),
            agent,
            shadows,
        ),
    );

    // (a) Never started: a safe, bounded no-op (nothing to cancel/join).
    assert_eq!(index.worker_status().state, WorkerState::NotStarted);
    run_shutdown(&tasks, &index).await;
    assert_eq!(index.worker_status().state, WorkerState::NotStarted);

    // (b) Active worker: attach kicks the owned reconciliation worker;
    // the shutdown sequence joins it within the service bound and the
    // owner is left terminal — a second shutdown finds nothing detached.
    index.attach(ws).unwrap();
    wait_state(&index, WorkerState::Running).await;
    let started = std::time::Instant::now();
    run_shutdown(&tasks, &index).await;
    assert_eq!(
        index.worker_status().state,
        WorkerState::Stopped,
        "the joined worker must be terminal: {:?}",
        index.worker_status()
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the index worker join must stay within the bound: {:?}",
        started.elapsed()
    );
    assert_eq!(
        index.shutdown_worker().await,
        WorkerShutdown::NotRunning,
        "no detached index task may outlive the shutdown sequence"
    );
}

/// The daemon shutdown sequence also JOINS the runtime's OWN lazily
/// hosted index worker: a runtime that never opened one is an inert,
/// bounded no-op (the sequence must not open it as a side effect), and a
/// runtime that hosted its service during an ordinary turn leaves no
/// ghost worker behind — the owned worker is cancelled and joined within
/// the bound, and a repeat accessor call finds nothing detached. The
/// sequence stays bounded in both cases.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_shutdown_joins_the_runtime_index_worker_bounded() {
    use faktor_index::{WorkerShutdown, WorkerState};

    async fn run_shutdown(
        tasks: &Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
        agent: &Arc<AgentRuntime>,
    ) {
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            shutdown_serving_daemon(
                tasks,
                None,
                None,
                None,
                Some(agent),
                tokio::spawn(std::future::pending::<()>()),
                None,
                None,
                tokio::spawn(async {}),
            ),
        )
        .await
        .expect("the shutdown sequence must stay bounded");
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("lib.rs"), b"pub fn indexed() -> i64 { 1 }\n").unwrap();
    let session =
        SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
    let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
    let sid = session
        .create_session(ws, "runtime-index", "fake", "m")
        .unwrap()
        .id();

    let mut registry = ProviderRegistry::new();
    registry
        .try_register(faktor_provider::InstanceProvider::wrap(
            Arc::new(AlwaysOk) as Arc<dyn Provider>,
            "fake",
        ))
        .unwrap();
    let agent = test_agent(session.clone(), registry);
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
    let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
        session.clone(),
        dir.path().join("shadows"),
    )
    .unwrap();
    let tasks = Arc::new(
        faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            session.clone(),
            agent.clone(),
            shadows,
        ),
    );

    // (a) Never opened: the accessor is the inert `None` and the
    // shutdown sequence must not open the service it is joining.
    assert!(agent.index_service_worker_status().is_none());
    run_shutdown(&tasks, &agent).await;
    assert!(
        agent.index_service_worker_status().is_none(),
        "shutdown must never open the runtime's index service as a side effect"
    );

    // (b) Opened: an ordinary turn hosts the runtime's IndexService (and
    // kicks its owned reconciliation worker); the same sequence joins it
    // and the owner is left terminal, never detached.
    agent.run_turn(sid, "hello", &[]).await.unwrap();
    assert!(
        agent.index_service_worker_status().is_some(),
        "the turn must have hosted the runtime index service"
    );
    let started = std::time::Instant::now();
    run_shutdown(&tasks, &agent).await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the runtime index worker join must stay within the bound: {:?}",
        started.elapsed()
    );
    let status = agent
        .index_service_worker_status()
        .expect("the service stays opened once hosted");
    assert_ne!(
        status.state,
        WorkerState::Running,
        "no ghost runtime index worker may survive the shutdown: {status:?}"
    );
    assert_eq!(
        agent.shutdown_index_service().await,
        Some(WorkerShutdown::NotRunning),
        "no detached runtime index task may outlive the shutdown sequence"
    );
}

/// Enterprise-disabled parity: a daemon with `[enterprise] enabled =
/// false` (and with an absent section) creates NO enterprise database;
/// an enabled section (which requires `[cloud]`, the principal source)
/// opens the durable retention/audit database and creates its
/// append-only ledger table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enterprise_disabled_daemon_creates_no_enterprise_db_and_enabled_opens_it() {
    let enabled_json = r#"{
            "model": "m",
            "cloud": {"enabled": true, "database": "cp.db", "scm_database": "repos.db"},
            "enterprise": {
                "enabled": true,
                "database": "ent.db",
                "organization": "org_local"
            }
        }"#;
    for (label, config_json, expect_enterprise) in [
        ("absent", r#"{"model": "m"}"#.to_string(), false),
        (
            "disabled",
            r#"{"model": "m", "enterprise": {"enabled": false}}"#.to_string(),
            false,
        ),
        ("enabled", enabled_json.to_string(), true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("faktor-plus.json");
        std::fs::write(&config, &config_json).unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, dir2, Some(config), Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        let enterprise_db = dir.path().join("ent.db");
        let default_enterprise_db = dir.path().join("enterprise.db");
        if expect_enterprise {
            assert!(
                enterprise_db.exists(),
                "{label}: an enabled [enterprise] section must open its database"
            );
            let conn = rusqlite::Connection::open(&enterprise_db).unwrap();
            let tables: Vec<String> = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(
                tables.iter().any(|name| name == "ent_audit_event"),
                "{label}: the append-only audit ledger table exists"
            );
            let audit_rows: i64 = conn
                .query_row("SELECT COUNT(*) FROM ent_audit_event", [], |row| row.get(0))
                .unwrap();
            assert_eq!(audit_rows, 0, "{label}: a clean boot mints no audit rows");
        } else {
            assert!(
                !enterprise_db.exists() && !default_enterprise_db.exists(),
                "{label}: a disabled [enterprise] section must create no database"
            );
        }
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(60), daemon)
            .await
            .expect("daemon shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }
}

/// Worker-mode disabled parity: the default `[worker_node]` section (and
/// every pre-existing config) makes `faktor worker run` refuse typed
/// before any network, database or workspace effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_node_disabled_refuses_before_any_effect() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let err = worker_node::run(None, data.clone(), None)
        .await
        .unwrap_err();
    assert!(err.contains("disabled"), "{err}");
    let err = worker_node::run(Some(4), data.clone(), None)
        .await
        .unwrap_err();
    assert!(err.contains("disabled"), "{err}");
    assert!(!data.exists(), "the disabled entry creates nothing at all");
}

/// End-to-end fake-transport round trip: the TaskExecutor places a run
/// remotely (real worker plane + placement seam), the worker-side runtime
/// claims the scheduler's lease, executes through a scripted (self-
/// verified, read-only) executor, submits the digest-bound result and the
/// landed result settles the PARENT run through the SAME TaskExecutor
/// settlement pipeline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_transport_round_trip_settles_the_parent_run() {
    struct YieldSleeper;
    impl faktor_worker::WorkerSleeper for YieldSleeper {
        fn sleep_ms(&self, _ms: i64) {
            std::thread::yield_now();
        }
    }
    struct ScriptedExecutor;
    impl faktor_worker::JobExecutor for ScriptedExecutor {
        fn execute(
            &self,
            _request: faktor_worker::JobExecutionRequest<'_>,
            _control: &faktor_worker::ExecutionControl,
        ) -> Result<faktor_worker::JobExecutionResult, String> {
            Ok(faktor_worker::JobExecutionResult {
                succeeded: true,
                self_verified: true,
                produced_digest: None,
                detail: String::new(),
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("owner")).unwrap();
    let graph = build_daemon(&data, None).unwrap();
    let clock = Arc::new(faktor_cloud::ManualClock::new(1_700_000_000_000));
    let organization = faktor_cloud::OrganizationId::try_new("org_local").unwrap();
    let plane = faktor_worker::WorkerPlane::new(
        Arc::new(faktor_worker::MemoryWorkerStore::new()),
        clock.clone(),
    );
    let requirements = faktor_worker::JobRequirements {
        os: None,
        arch: None,
        toolchains: vec!["rust".into()],
        sandbox: vec![],
        network: None,
        min_cpu_cores: 0,
        min_memory_mb: 0,
        gpu: false,
        region: None,
        trust_domain: "org_local".into(),
    };
    graph
        .tasks
        .set_worker_placement(faktor_orchestrator::placement::WorkerPlacement::enabled(
            Arc::new(WorkerPlaneAdapter {
                plane: plane.clone(),
                organization: organization.clone(),
                trust_domain: "org_local".into(),
                requirements,
            }),
        ));

    let goal = "e2e remote goal";
    let payload_digest = blake3::hash(goal.as_bytes()).to_hex().to_string();
    let issued = plane
        .mint_registration_token(&organization, "org_local", "e2e")
        .unwrap();
    let token = faktor_cloud::SecretToken::try_new(issued.token.expose().to_string()).unwrap();
    let worker_id = faktor_worker::WorkerId::try_new("wrk_e2e").unwrap();
    let transport = Arc::new(faktor_worker::InProcessTransport::new(
        plane.clone(),
        organization.clone(),
    ));
    transport.bind_token(&worker_id, token.clone());
    transport.stage_payload(&payload_digest, "text/plain", goal);
    let workspace_root = data.join("worker_workspaces");
    std::fs::create_dir_all(&workspace_root).unwrap();
    let runtime = faktor_worker::WorkerRuntime::new(
        faktor_worker::WorkerRuntimeConfig {
            enabled: true,
            worker_id: worker_id.clone(),
            display_name: "e2e".into(),
            capabilities: plane
                .register(
                    &organization,
                    &worker_id,
                    &token,
                    faktor_worker::WorkerCapabilities {
                        os: std::env::consts::OS.into(),
                        arch: std::env::consts::ARCH.into(),
                        toolchains: vec!["rust".into()],
                        sandbox: vec![],
                        network: faktor_worker::NetworkProfile::None,
                        cpu_cores: 1,
                        memory_mb: 1_024,
                        gpu: None,
                        region: "local".into(),
                        trust_domain: "org_local".into(),
                        protocol_version: faktor_worker::WORKER_PROTOCOL_VERSION,
                    },
                    "e2e",
                )
                .unwrap()
                .worker
                .capabilities,
            token: token.clone(),
            claim_deadline_ms: 0,
            claim_interval_ms: 10,
            heartbeat_interval_ms: 1_000,
            discard_workspace_on_success: true,
        },
        transport,
        Arc::new(ScriptedExecutor),
        Arc::new(YieldSleeper),
        clock.clone(),
        workspace_root,
    )
    .unwrap();

    let ws = graph
        .session
        .create_workspace(data.join("owner").to_str().unwrap())
        .unwrap();
    let parent = graph
        .session
        .create_session(ws, "e2e owner", "fake", "default")
        .unwrap()
        .id();
    let receipt = graph
        .tasks
        .start_task(
            parent,
            faktor_orchestrator::runtime::task_executor::TaskRunRequest {
                goal: goal.into(),
                work_items: vec![faktor_orchestrator::WorkItem::new(
                    "w1",
                    "remote work",
                    faktor_orchestrator::WorkKind::Analysis,
                )],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        receipt.mode,
        faktor_orchestrator::runtime::task_executor::TaskRunMode::Remote
    );
    let job_id = receipt.run_id.clone();

    // claim (adopts the scheduler's placement lease) -> execute -> submit
    let outcome = runtime.run_once().unwrap();
    match outcome {
        faktor_worker::RunOutcome::Completed {
            job_id: completed,
            generation,
            submit,
            ..
        } => {
            assert_eq!(completed, job_id);
            assert_eq!(generation, 1);
            assert_eq!(submit, faktor_worker::ResultOutcome::Landed);
        }
        other => panic!("expected a landed completion, got {other:?}"),
    }
    let job = faktor_worker::ExecutionJobId::try_new(job_id.clone()).unwrap();
    let status = plane.job_status(&organization, &job).unwrap();
    assert_eq!(status.job.state, faktor_worker::JobState::Completed);
    assert!(status.result.is_some());

    // the parent carries a durable task row (the completion-step proof
    // read requires it); the run itself never executed locally.
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id,
            session_id: parent,
            goal: goal.into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let completion = faktor_orchestrator::remote_completion::RemoteRunCompletion {
        parent,
        run_id: job_id.clone(),
        job_id: job_id.clone(),
        generation: 1,
        kind: "in_session".into(),
        digest: payload_digest.clone(),
        outcome: faktor_orchestrator::remote_completion::RemoteRunOutcome::Succeeded,
        claim: faktor_orchestrator::remote_completion::RemoteVerificationClaim {
            self_verified: true,
            produced_digest: None,
        },
    };
    match graph.tasks.complete_remote_run(completion).await.unwrap() {
        faktor_orchestrator::remote_completion::RemoteCompletionOutcome::Settled {
            class,
            settlement,
        } => {
            assert_eq!(
                class,
                faktor_orchestrator::remote_completion::RemoteCompletionClass::SelfVerifiedReadOnly
            );
            assert_eq!(settlement.run_id, job_id);
        }
        other => panic!("expected a settlement, got {other:?}"),
    }
}

#[test]
fn daemon_outbound_scan_registers_every_configured_commerce_credential_value() {
    const KEY_ENV: &str = "FAKTOR_TEST_CLI_COMMERCE_SCAN_KEY";
    const ID_ENV: &str = "FAKTOR_TEST_CLI_COMMERCE_SCAN_ID";
    const SECRET_ENV: &str = "FAKTOR_TEST_CLI_COMMERCE_SCAN_SECRET";
    // Values must not trip the frozen GENERIC patterns (sk-*, AKIA, …):
    // only the configured-secret registry can catch them, so a hit proves
    // the commerce credential VALUE was registered by name resolution.
    const KEY: &str = "kp-commerce-key-51ab";
    const ID: &str = "kp-commerce-id-77c1";
    const PAIR: &str = "kp-commerce-secret-0d42";
    std::env::set_var(KEY_ENV, KEY);
    std::env::set_var(ID_ENV, ID);
    std::env::set_var(SECRET_ENV, PAIR);
    let mut cfg = config::Config::default();
    cfg.commerce.enabled = true;
    cfg.commerce.connectors.mouser = Some(config::CommerceApiConnectorCfg {
        enabled: true,
        api_key_env: Some(KEY_ENV.to_string()),
    });
    cfg.commerce.connectors.digikey = Some(config::CommerceDigikeyConnectorCfg {
        enabled: true,
        client_id_env: Some(ID_ENV.to_string()),
        client_secret_env: Some(SECRET_ENV.to_string()),
    });
    let scan = daemon_outbound_scan(&cfg);
    let registry = scan.registry.expect("registry installed");
    for value in [KEY, ID, PAIR] {
        assert!(
            !registry.scan_exact(value.as_bytes()).is_empty(),
            "configured commerce credential {value} must be registered"
        );
    }
    // Disabled commerce resolves the connectors away: nothing registers.
    cfg.commerce.enabled = false;
    let scan = daemon_outbound_scan(&cfg);
    let registry = scan.registry.expect("registry installed");
    assert!(registry.scan_exact(KEY.as_bytes()).is_empty());
    assert!(registry.scan_exact(PAIR.as_bytes()).is_empty());
    std::env::remove_var(KEY_ENV);
    std::env::remove_var(ID_ENV);
    std::env::remove_var(SECRET_ENV);
}

/// The builder's single-registry contract: the EGRESS scan config, the
/// supervisor's artifact filter and the runtime's tool-output scrubber are
/// all built from ONE `Arc<SecretRegistry>` — the exact expressions
/// `build_daemon_core` uses. A same-value planted in one must be caught by
/// all three.
#[test]
fn builder_shares_one_configured_secret_registry_across_egress_tool_and_artifact_filter() {
    use faktor_terminal::ArtifactSecretFilter as _;
    const KEY_ENV: &str = "FAKTOR_TEST_CLI_SHARED_SECRET_KEY";
    // Pattern-invisible: only the configured registry can catch this value.
    const KEY: &str = "kp-shared-secret-7f21";
    assert!(
        faktor_security::scan_secrets(KEY, &faktor_security::SecretPolicy::default()).is_empty()
    );
    std::env::set_var(KEY_ENV, KEY);
    let mut cfg = config::Config::default();
    cfg.commerce.enabled = true;
    cfg.commerce.connectors.mouser = Some(config::CommerceApiConnectorCfg {
        enabled: true,
        api_key_env: Some(KEY_ENV.to_string()),
    });

    let registry = daemon_secret_registry(&cfg);
    // (1) Egress: the scan config carries the SAME instance.
    let egress = outbound_scan_config(registry.clone());
    let egress_registry = egress.registry.expect("egress registry installed");
    assert!(
        Arc::ptr_eq(&egress_registry, &registry),
        "the egress scan must share the one configured-secret registry instance"
    );
    assert!(!egress_registry
        .scan_exact(format!("Bearer {KEY}").as_bytes())
        .is_empty());
    // (2) Supervisor artifact filter: same instance, exact ranges.
    let filter = RegistryArtifactFilter::new(registry.clone());
    assert_eq!(filter.max_match_len(), KEY.len());
    assert_eq!(
        filter.matches(format!("x{KEY}y").as_bytes()),
        vec![(1, KEY.len())]
    );
    // (3) The runtime registry (what AgentDeps receives) is behaviorally the
    // same configured set — asserted on a BUILT daemon in the test below.
    std::env::remove_var(KEY_ENV);
}

/// Built-daemon end-to-end: [providers] key -> the ONE registry -> the
/// egress gate hard-blocks a request body carrying it (and the server is
/// never contacted), while the runtime's `secret_registry` — the same
/// configured set the tool/CAS scrubbers use — exact-scans the value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn built_daemon_egress_and_tool_registry_share_the_configured_provider_key() {
    const KEY_ENV: &str = "FAKTOR_TEST_CLI_BUILT_SHARED_SECRET_KEY";
    const KEY: &str = "kp-built-secret-31ad";
    std::env::set_var(KEY_ENV, KEY);
    let server = MockServer::new();
    let base = server.base_url().await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let allow_row = format!("http://127.0.0.1:{port}");
    let dir = tempfile::tempdir().unwrap();
    let cfg = egress_cfg(
        dir.path(),
        "shared-secret.json",
        &base,
        Some(KEY_ENV),
        Some(std::slice::from_ref(&allow_row)),
    );
    let graph = build_daemon(dir.path(), Some(cfg)).unwrap();

    // Egress: the body carries the configured value -> typed hard block
    // BEFORE any connect (the allowlisted server sees nothing).
    let err = chat_text(graph.providers.get("mocked").unwrap(), KEY)
        .await
        .expect_err("the configured key must be egress-blocked");
    assert!(
        err.message.contains("configured_secret"),
        "the egress block must name the configured-secret registry: {}",
        err.message
    );
    assert_eq!(server.request_count(), 0, "blocked before connect");

    // Tool/CAS half: the runtime was given the SAME configured set.
    let tool_registry = graph
        .agent
        .deps()
        .secret_registry
        .clone()
        .expect("the builder must wire the runtime secret registry");
    assert!(
        !tool_registry.scan_exact(KEY.as_bytes()).is_empty(),
        "the tool scrubber must know the configured provider key"
    );
    assert!(tool_registry.scan_exact(b"unrelated-value").is_empty());
    std::env::remove_var(KEY_ENV);
}
