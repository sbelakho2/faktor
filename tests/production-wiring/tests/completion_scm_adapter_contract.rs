//! MANUAL adapter-contract certification — the explicit exception to the
//! production-wiring authority-replacement static scan
//! (`production_wiring_tests_never_replace_built_daemon_authorities` in
//! `faktor-tests-static-authority`): embedded hosts (and this contract test)
//! may install a hand-built canonical SCM adapter onto a built executor
//! through [`faktor_orchestrator::runtime::task_executor::TaskExecutor::set_completion_scm_provider`].
//!
//! This file certifies that MANUAL seam against the real
//! `faktor_scm::GitHubCompletionScm` + `faktor_scm::GitHubApp` adapter over a
//! loopback fake GitHub server:
//!
//! 1. a built executor with NO provider records the explicit
//!    `native_pr_scm_not_configured` blocker for a contracted PR step;
//! 2. after `set_completion_scm_provider(Some(adapter))` the SAME executor
//!    runs the contracted step through the adapter (installation-token
//!    bearer, exact refs payload, PR identity);
//! 3. after `set_completion_scm_provider(None)` the blocker is back.
//!
//! The CONFIG-driven daemon wiring (`[cloud.github_app]` ->
//! `build_daemon_core` -> executor) is certified separately in
//! `completion_scm.rs`; this test covers only the manual embedded-host seam.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use faktor_core::completion::{CompletionContract, CompletionStep, CompletionStepOutcome};
use faktor_core::id::VerificationRecordId;
use faktor_core::state::{TaskState, TaskTransition, VerificationStatus};
use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_scm::completion::GitHubCompletionScm;
use faktor_scm::github::GitHubAppConfig;
use faktor_scm::store::{MemoryScmStore, RepositoryRow, ScmStore};
use faktor_scm::{GitHubApp, ManualClock, StaticTokenSource};
use faktor_session::{Task, TaskBudget};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use faktor_tests_production_wiring::wiring::{self, Config};

const TENANT: &str = "tenant-a";
const NOW_MS: i64 = 1_700_000_000_000;

// ------------------------------------------------------------------- mock

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
        }
    }

    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }
}

#[derive(Clone)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct MockState {
    routes: HashMap<(String, String), VecDeque<Reply>>,
    requests: Vec<Recorded>,
}

struct MockServer {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(MockState::default()));
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let state = task_state.clone();
                tokio::spawn(async move {
                    let _ = handle_conn(&mut socket, &state).await;
                });
            }
        });
        Self { addr, state, task }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn push(&self, method: &str, path: &str, reply: Reply) {
        self.state
            .lock()
            .expect("mock state")
            .routes
            .entry((method.to_string(), path.to_string()))
            .or_default()
            .push_back(reply);
    }

    fn requests(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.state
            .lock()
            .expect("mock state")
            .requests
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .cloned()
            .collect()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_conn(
    socket: &mut tokio::net::TcpStream,
    state: &Arc<Mutex<MockState>>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 128 * 1024 {
            return Ok(());
        }
    };
    let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let path = target.split('?').next().unwrap_or_default().to_string();
    let mut content_length = 0usize;
    let mut request_headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            request_headers.push((name, value));
        }
    }
    while buf.len() < header_end + content_length {
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body =
        String::from_utf8_lossy(&buf[header_end..(header_end + content_length).min(buf.len())])
            .to_string();
    let reply = {
        let mut state = state.lock().expect("mock state");
        state.requests.push(Recorded {
            method: method.clone(),
            path: path.clone(),
            headers: request_headers,
            body,
        });
        state
            .routes
            .get_mut(&(method.clone(), path.clone()))
            .and_then(|queue| queue.pop_front())
    };
    let reply = reply.unwrap_or_else(|| Reply::status(404));
    let reason = match reply.status {
        200 => "OK",
        201 => "Created",
        404 => "Not Found",
        _ => "X",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(reply.body.as_bytes()).await?;
    Ok(())
}

// ---------------------------------------------------------------- helpers

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repository_row(owner: &str, name: &str, installation_id: i64) -> RepositoryRow {
    RepositoryRow {
        id: 0,
        installation_id,
        organization_id: TENANT.to_string(),
        owner: owner.to_string(),
        name: name.to_string(),
        full_name: format!("{owner}/{name}"),
        default_branch: "main".to_string(),
        private: false,
        archived: false,
        updated_ms: NOW_MS,
    }
}

/// Seed a contracted task exactly as the executor's proof-validated API
/// requires: a non-terminal task at `Verifying`, the `include_pr` contract
/// at the current revision, and the durable PASSED verification record that
/// authorizes the step.
fn seed_contract_task(
    graph: &wiring::DaemonGraph,
    parent: faktor_core::SessionId,
) -> VerificationRecordId {
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(Task {
            task_id,
            session_id: parent,
            goal: "ship the manually wired PR".to_string(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    for target in [
        TaskTransition::StartRunning,
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let rev = handle.task_revision(task_id).unwrap();
        handle.transition_task(task_id, rev, target, None).unwrap();
    }
    let rev = handle.task_revision(task_id).unwrap();
    handle
        .set_completion_contract(
            task_id,
            rev,
            CompletionContract {
                include_commit: false,
                include_push: false,
                include_pr: true,
            },
        )
        .unwrap();
    handle
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            handle.now_ms(),
        )
        .unwrap()
}

/// One fresh session over `repo` with the session-owned worktree row the
/// completion step resolves its root from.
fn session_for(graph: &wiring::DaemonGraph, repo: &Path, label: &str) -> faktor_core::SessionId {
    let workspace = graph
        .session
        .create_workspace(repo.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, label, "provider", "model")
        .unwrap();
    let parent = handle.id();
    graph.session.ensure_owner_worktree(parent).unwrap();
    parent
}

async fn run_and_expect_blocker(graph: &wiring::DaemonGraph, parent: faktor_core::SessionId) {
    let proof = seed_contract_task(graph, parent);
    let report = graph
        .tasks
        .run_completion_steps(parent, proof)
        .await
        .expect("the executor runs the contracted steps")
        .expect("include_pr contract is requested");
    assert_eq!(
        report.outcome_of(CompletionStep::Pr),
        Some(CompletionStepOutcome::Failed),
        "{report:?}"
    );
    let detail = &report
        .records
        .iter()
        .find(|record| record.step == CompletionStep::Pr)
        .expect("a PR step record")
        .detail;
    assert!(
        detail.contains("native_pr_scm_not_configured"),
        "the fail-closed configuration blocker must be explicit: {detail}"
    );
}

// ------------------------------------------------------------------- test

/// The MANUAL adapter-contract seam (embedded hosts): a hand-built
/// `GitHubCompletionScm`/`GitHubApp` adapter installed through
/// `set_completion_scm_provider` runs a contracted PR step through the real
/// adapter; clearing it restores the explicit configuration blocker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_completion_scm_adapter_contract_and_embedded_host_injection() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=faktor@example.test",
            "-c",
            "user.name=Faktor",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    git(&repo, &["checkout", "-q", "-b", "feature/x"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let server = MockServer::start().await;
    server.push(
        "GET",
        "/repos/acme/widgets/git/ref/heads/feature/x",
        Reply::status(404),
    );
    server.push(
        "POST",
        "/repos/acme/widgets/git/refs",
        Reply::json(
            201,
            serde_json::json!({
                "ref": "refs/heads/feature/x",
                "object": { "sha": head },
                "url": "https://example.test/acme/widgets/git/refs/heads/feature/x",
            }),
        ),
    );
    server.push(
        "GET",
        "/repos/acme/widgets/pulls",
        Reply::json(200, serde_json::json!([])),
    );
    server.push(
        "POST",
        "/repos/acme/widgets/pulls",
        Reply::json(
            201,
            serde_json::json!({
                "number": 42,
                "state": "open",
                "html_url": "https://example.test/acme/widgets/pull/42",
                "updated_at": "2026-01-01T00:00:00Z",
                "head": { "ref": "feature/x", "sha": head },
                "base": { "ref": "main" },
                "body": "",
            }),
        ),
    );

    // The daemon graph with the DEFAULT config: no `[cloud]`, so the builder
    // wires no completion SCM provider and opens no SCM store.
    let data = dir.path().join("data");
    let graph =
        wiring::build_production_graph(&data, Config::default()).expect("production daemon graph");
    assert!(
        graph.scm.is_none(),
        "cloud-disabled parity: no SCM store is opened"
    );

    // 1. No provider: the contracted PR step is the explicit blocker.
    let session_blocked = session_for(&graph, &repo, "manual blocked");
    run_and_expect_blocker(&graph, session_blocked).await;

    // The hand-built canonical adapter over a loopback fake GitHub and a
    // test-owned synced store.
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    store
        .upsert_repository(&repository_row("acme", "widgets", 7))
        .expect("synced repository row");
    let app = GitHubApp::new(
        GitHubAppConfig {
            api_base: server.base(),
            ..Default::default()
        },
        Arc::new(PolicyCheckedHttpTransport::permissive()),
        Arc::new(StaticTokenSource::minimal(i64::MAX).expect("token source")),
        store.clone(),
        Arc::new(ManualClock::new(NOW_MS)),
    )
    .expect("github app adapter");
    let adapter =
        GitHubCompletionScm::new(Arc::new(app), store, TENANT).expect("completion adapter");
    graph
        .tasks
        .set_completion_scm_provider(Some(Arc::new(adapter)));

    // 2. Installed manually: the SAME executor now runs the contracted step
    // through the real adapter.
    let session_wired = session_for(&graph, &repo, "manual wired");
    let proof = seed_contract_task(&graph, session_wired);
    let report = graph
        .tasks
        .run_completion_steps(session_wired, proof)
        .await
        .expect("the executor runs the contracted steps")
        .expect("include_pr contract is requested");
    assert!(report.all_succeeded(), "{report:?}");
    assert_eq!(
        report.pr_url.as_deref(),
        Some("https://example.test/acme/widgets/pull/42")
    );
    let refs = server.requests("POST", "/repos/acme/widgets/git/refs");
    assert_eq!(refs.len(), 1, "the branch is created through GitHubApp");
    assert_eq!(
        refs[0].header("authorization"),
        Some("Bearer test-installation-token"),
        "the canonical adapter minted/presented the installation token"
    );
    let refs_body: serde_json::Value = serde_json::from_str(&refs[0].body).unwrap();
    assert_eq!(refs_body["ref"], "refs/heads/feature/x");
    assert_eq!(refs_body["sha"], head, "created at the verified head");
    let prs = server.requests("POST", "/repos/acme/widgets/pulls");
    assert_eq!(prs.len(), 1, "the PR is created through GitHubApp");
    let pr_body: serde_json::Value = serde_json::from_str(&prs[0].body).unwrap();
    assert_eq!(pr_body["head"], "feature/x");
    assert_eq!(pr_body["base"], "main");

    // 3. Cleared: the blocker is back (the clear rebuilt the runner).
    graph.tasks.set_completion_scm_provider(None);
    let session_cleared = session_for(&graph, &repo, "manual cleared");
    run_and_expect_blocker(&graph, session_cleared).await;
}
