//! Contract tests of the completion-step SCM adapter against a loopback
//! GitHub mock: the completion request goes through the canonical
//! [`GitHubApp`] adapter (installation resolution from the synced rows,
//! branch reconciliation, PR create/reconcile) over the daemon's checked
//! transport. Nothing here talks to the network or carries credentials.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_scm::completion::{
    CompletionPrRequest, CompletionScm, GitHubCompletionScm, COMPLETION_REPOSITORY_PAGE,
};
use faktor_scm::github::GitHubAppConfig;
use faktor_scm::ids::ExternalOperationId;
use faktor_scm::store::{MemoryScmStore, RepositoryRow, ScmStore};
use faktor_scm::{GitHubApp, ManualClock, ScmError, StaticTokenSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NOW_MS: i64 = 1_700_000_000_000;
const TENANT: &str = "tenant-a";
const HEAD: &str = "1111111111111111111111111111111111111111";
const OPERATION: &str = "task:t-1:rev:2:github:pull_request";
const MARKER: &str = "faktor:task:t-1:rev:2";
const PR_BODY: &str = "Created by Faktor task t-1. Reconciliation marker: faktor:task:t-1:rev:2";

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
    query: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
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
        let state = Arc::new(Mutex::new(MockState::default()));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("mock bind");
        let addr = listener.local_addr().expect("mock addr");
        let serve_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let state = serve_state.clone();
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

    fn request_count(&self) -> usize {
        self.state.lock().expect("mock state").requests.len()
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
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
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
            query,
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
        422 => "Unprocessable Entity",
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

// ------------------------------------------------------------------ helpers

struct Harness {
    server: MockServer,
    adapter: GitHubCompletionScm,
    store: Arc<dyn ScmStore>,
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

async fn harness_with_store(store: Arc<dyn ScmStore>) -> Harness {
    let server = MockServer::start().await;
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let app = GitHubApp::new(
        GitHubAppConfig {
            api_base: server.base(),
            ..Default::default()
        },
        Arc::new(PolicyCheckedHttpTransport::permissive()),
        Arc::new(
            StaticTokenSource::minimal(NOW_MS.saturating_add(3_600_000)).expect("token source"),
        ),
        store.clone(),
        clock,
    )
    .expect("adapter");
    let adapter =
        GitHubCompletionScm::new(Arc::new(app), store.clone(), TENANT).expect("completion adapter");
    Harness {
        server,
        adapter,
        store,
    }
}

async fn harness() -> Harness {
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    store
        .upsert_repository(&repository_row("acme", "widgets", 7))
        .expect("synced repository row");
    harness_with_store(store).await
}

fn request() -> CompletionPrRequest {
    CompletionPrRequest {
        operation_id: ExternalOperationId::try_new(OPERATION).unwrap(),
        organization: "acme".into(),
        repository: "widgets".into(),
        branch: "feature/x".into(),
        base: "main".into(),
        head_sha: HEAD.into(),
        marker: MARKER.into(),
        title: "faktor: ship the completion adapter".into(),
        body: PR_BODY.into(),
    }
}

fn script_branch_absent(server: &MockServer) {
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
                "object": { "sha": HEAD },
                "url": "https://example.test/acme/widgets/git/refs/heads/feature/x",
            }),
        ),
    );
}

fn pull_request_json() -> serde_json::Value {
    serde_json::json!({
        "number": 41,
        "state": "open",
        "html_url": "https://example.test/acme/widgets/pull/41",
        "updated_at": "2026-01-01T00:00:00Z",
        "head": { "ref": "feature/x", "sha": HEAD },
        "base": { "ref": "main" },
        "body": PR_BODY,
    })
}

fn script_pr_created(server: &MockServer) {
    server.push(
        "GET",
        "/repos/acme/widgets/pulls",
        Reply::json(200, serde_json::json!([])),
    );
    server.push(
        "POST",
        "/repos/acme/widgets/pulls",
        Reply::json(201, pull_request_json()),
    );
}

// ------------------------------------------------------------------- tests

/// The completion request drives the REAL canonical adapter: the synced
/// installation is resolved, the branch is created at the exact verified
/// head, the PR is created with the exact title/body/head/base, and the
/// returned identities map back (number/version/url, marker).
#[tokio::test]
async fn completion_pr_reconciles_through_the_canonical_github_app() {
    let harness = harness().await;
    script_branch_absent(&harness.server);
    script_pr_created(&harness.server);
    let result = harness
        .adapter
        .reconcile_completion_pr(&request())
        .await
        .expect("reconciliation");
    assert_eq!(result.repository.installation().raw(), 7);
    assert_eq!(result.repository.full_name(), "acme/widgets");
    assert_eq!(result.branch.head_sha, HEAD);
    assert!(result.branch.created, "the branch was created by this call");
    assert_eq!(result.pull_request.reference.number(), 41);
    assert_eq!(
        result.pull_request.version,
        format!("{HEAD}@2026-01-01T00:00:00Z")
    );
    assert_eq!(
        result.pull_request.url,
        "https://example.test/acme/widgets/pull/41"
    );
    assert_eq!(result.pull_request.marker, MARKER);
    assert!(
        result.pull_request.created,
        "the PR was created by this call"
    );

    // Branch/ref mapping: the POST names the canonical ref and exact sha.
    let refs = harness
        .server
        .requests("POST", "/repos/acme/widgets/git/refs");
    assert_eq!(refs.len(), 1);
    let refs_body: serde_json::Value = serde_json::from_str(&refs[0].body).unwrap();
    assert_eq!(refs_body["ref"], "refs/heads/feature/x");
    assert_eq!(refs_body["sha"], HEAD);
    assert_eq!(
        refs[0].header("authorization"),
        Some("Bearer test-installation-token"),
        "the canonical adapter minted/presented the installation token"
    );

    // PR body/head/base mapping: the request strings reach the provider
    // verbatim, and the reconciliation lookup query is the exact identity.
    let creates = harness.server.requests("POST", "/repos/acme/widgets/pulls");
    assert_eq!(creates.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&creates[0].body).unwrap();
    assert_eq!(body["title"], "faktor: ship the completion adapter");
    assert_eq!(body["body"], PR_BODY);
    assert_eq!(body["head"], "feature/x");
    assert_eq!(body["base"], "main");
    let lookups = harness.server.requests("GET", "/repos/acme/widgets/pulls");
    assert_eq!(lookups.len(), 1);
    assert!(
        lookups[0].query.contains("head=acme") && lookups[0].query.contains("feature"),
        "the lookup is keyed by the exact head: {}",
        lookups[0].query
    );
    assert!(
        lookups[0].query.contains("base=main"),
        "the lookup is keyed by the exact base: {}",
        lookups[0].query
    );

    // The canonical provider journaled the operation identity durably.
    let row = harness
        .store
        .external_operation(OPERATION)
        .expect("store read")
        .expect("operation row");
    assert_eq!(row.kind, "pull_request");
    assert_eq!(row.state, "completed");
    assert_eq!(row.external_id.as_deref(), Some("41"));
    let expected_version = format!("{HEAD}@2026-01-01T00:00:00Z");
    assert_eq!(row.version.as_deref(), Some(expected_version.as_str()));
}

/// A lost response (crash between the remote create and the durable record)
/// is reconciled by a repeated call: the EXISTING branch and the EXISTING
/// marker-carrying PR are returned and nothing is created twice.
#[tokio::test]
async fn completion_pr_replay_reconciles_without_duplicates() {
    let harness = harness().await;
    script_branch_absent(&harness.server);
    script_pr_created(&harness.server);
    let first = harness
        .adapter
        .reconcile_completion_pr(&request())
        .await
        .expect("first reconciliation");
    assert!(first.pull_request.created);

    // The restart sees the branch and the PR the first call created.
    harness.server.push(
        "GET",
        "/repos/acme/widgets/git/ref/heads/feature/x",
        Reply::json(200, serde_json::json!({ "object": { "sha": HEAD } })),
    );
    harness.server.push(
        "GET",
        "/repos/acme/widgets/pulls",
        Reply::json(200, serde_json::json!([pull_request_json()])),
    );
    let replay = harness
        .adapter
        .reconcile_completion_pr(&request())
        .await
        .expect("replay reconciliation");
    assert_eq!(replay.branch.head_sha, HEAD);
    assert!(!replay.branch.created, "the branch already existed");
    assert_eq!(replay.pull_request.reference.number(), 41);
    assert!(!replay.pull_request.created, "the PR already existed");
    assert_eq!(replay.pull_request.marker, MARKER);
    assert_eq!(
        harness
            .server
            .requests("POST", "/repos/acme/widgets/pulls")
            .len(),
        1,
        "the replay must not create a duplicate PR"
    );
    assert_eq!(
        harness
            .server
            .requests("POST", "/repos/acme/widgets/git/refs")
            .len(),
        1,
        "the replay must not create a duplicate branch"
    );
}

/// No synced repository row for the requested owner/name is an explicit
/// configuration blocker (the installation cannot be resolved), and the
/// provider is never contacted.
#[tokio::test]
async fn completion_pr_without_a_synced_repository_is_a_config_blocker() {
    let harness = harness_with_store(Arc::new(MemoryScmStore::new())).await;
    let err = harness
        .adapter
        .reconcile_completion_pr(&request())
        .await
        .expect_err("no synced repository row");
    match err {
        ScmError::Config(message) => {
            assert!(
                message.contains("no synced GitHub App repository acme/widgets"),
                "{message}"
            );
        }
        other => panic!("expected a typed config blocker, got {other:?}"),
    }
    assert_eq!(
        harness.server.request_count(),
        0,
        "the blocker is decided before any provider call"
    );
}

/// A branch that already resolves to a DIFFERENT head is a typed
/// reconciliation conflict: no PR is created on top of a moved branch.
#[tokio::test]
async fn completion_pr_branch_conflict_is_refused() {
    let harness = harness().await;
    harness.server.push(
        "GET",
        "/repos/acme/widgets/git/ref/heads/feature/x",
        Reply::json(
            200,
            serde_json::json!({ "object": { "sha": "2222222222222222222222222222222222222222" } }),
        ),
    );
    let err = harness
        .adapter
        .reconcile_completion_pr(&request())
        .await
        .expect_err("branch conflict");
    assert!(matches!(err, ScmError::ReconcileConflict { .. }), "{err:?}");
    assert!(
        harness
            .server
            .requests("GET", "/repos/acme/widgets/pulls")
            .is_empty(),
        "the PR lookup never runs after a branch conflict"
    );
    assert!(harness
        .server
        .requests("POST", "/repos/acme/widgets/pulls")
        .is_empty());
}

/// The installation lookup walks the bounded pages of the tenant's synced
/// rows: the match is found past the first full page without stopping early
/// or querying another tenant.
#[tokio::test]
async fn completion_pr_installation_lookup_paginates_the_synced_rows() {
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    for index in 0..COMPLETION_REPOSITORY_PAGE {
        store
            .upsert_repository(&repository_row(&format!("other-{index}"), "widgets", 7))
            .expect("filler row");
    }
    store
        .upsert_repository(&repository_row("acme", "widgets", 9))
        .expect("target row");
    let harness = harness_with_store(store).await;
    script_branch_absent(&harness.server);
    script_pr_created(&harness.server);
    let result = harness
        .adapter
        .reconcile_completion_pr(&request())
        .await
        .expect("pagination reconciliation");
    assert_eq!(
        result.repository.installation().raw(),
        9,
        "the match past the first page is found"
    );
}
