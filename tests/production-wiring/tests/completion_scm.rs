//! Production-wiring certification for the completion PR step: the DAEMON'S
//! `TaskExecutor` (built by `build_daemon_core`), configured exactly as
//! `serve_impl` configures it, executes a contracted `include_pr` task
//! through the REAL `faktor_scm::GitHubCompletionScm` over the real
//! `GitHubApp` adapter, against a loopback fake GitHub server.
//!
//! The assertions are on the ADAPTER PATH itself: the fake GitHub sees the
//! installation-token bearer, the exact refs payload at the verified head,
//! and the PR create with the orchestrator's head/base/marker — never a
//! test-double SCM provider.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use faktor_core::completion::CompletionContract;
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
    goal: &str,
) -> VerificationRecordId {
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(Task {
            task_id,
            session_id: parent,
            goal: goal.to_string(),
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

// ------------------------------------------------------------------- test

/// The daemon executor's contracted PR step runs the production
/// `GitHubCompletionScm`/`GitHubApp` adapter against the fake GitHub server:
/// the ref is created at the verified HEAD with the minted installation
/// token and the PR carries the exact head/base/marker identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_pr_goes_through_the_production_github_app_adapter() {
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

    // The daemon graph: the production TaskExecutor, session store and
    // supervisor the serve path builds.
    let data = dir.path().join("data");
    let graph =
        wiring::build_production_graph(&data, Config::default()).expect("production daemon graph");
    let workspace = graph
        .session
        .create_workspace(repo.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, "completion wiring", "provider", "model")
        .unwrap();
    let parent = handle.id();
    // The protocol surfaces' own owner-worktree adoption: the completion
    // step resolves the owner root from the session's durable worktree row.
    graph.session.ensure_owner_worktree(parent).unwrap();

    // The PRODUCTION SCM adapter (the same construction `serve_impl` wires),
    // over a permssive checked transport to the loopback fake GitHub.
    let server = MockServer::start().await;
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
        GitHubCompletionScm::new(Arc::new(app), store.clone(), TENANT).expect("completion adapter");
    graph
        .tasks
        .set_completion_scm_provider(Some(Arc::new(adapter)));

    // Script the canonical reconciliation: absent branch -> create ref ->
    // absent PR -> create PR.
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
                "number": 41,
                "state": "open",
                "html_url": "https://example.test/acme/widgets/pull/41",
                "updated_at": "2026-01-01T00:00:00Z",
                "head": { "ref": "feature/x", "sha": head },
                "base": { "ref": "main" },
                "body": "",
            }),
        ),
    );

    let proof = seed_contract_task(&graph, parent, "ship the wired PR");
    let report = graph
        .tasks
        .run_completion_steps(parent, proof)
        .await
        .expect("the daemon executor runs the contracted steps")
        .expect("include_pr contract is requested");
    assert!(report.all_succeeded(), "{report:?}");
    assert_eq!(
        report.pr_url.as_deref(),
        Some("https://example.test/acme/widgets/pull/41")
    );

    // The fake GitHub server proves the ADAPTER path: installation-token
    // bearer + exact ref at the verified head + PR create with the
    // orchestrator's identity.
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
    assert_eq!(pr_body["title"], "faktor: ship the wired PR");
    assert!(
        pr_body["body"]
            .as_str()
            .unwrap_or_default()
            .contains("Created by Faktor task"),
        "{pr_body}"
    );

    // The durable step row certifies through the adapter, not a local fact.
    let task_id = handle.task_id().unwrap();
    let revision = handle.task_revision(task_id).unwrap();
    let rows = handle
        .ledger_completion_step_statuses(task_id.raw(), revision.raw())
        .unwrap();
    let last = rows.last().expect("a durable completion step row");
    assert_eq!(
        last.status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    assert!(last.detail.contains("github"), "{last:?}");
}
