//! Adversarial integration tests of the GitHub App adapter and the durable
//! SCM rows, driven by a real loopback HTTP mock through the daemon's
//! checked transport. Covers: reconciliation idempotency under a lost
//! response (crash), brute reconcile of an existing branch, rate-limit
//! backoff with no hot loop, ETag revalidation serving 304 from cache, and
//! installation/repository sync + webhook dedupe across process restarts.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_scm::github::GitHubAppConfig;
use faktor_scm::ids::{
    ExternalOperationId, IssueRef, PullRequestRef, RemoteRef, RepositoryRef, ScmInstallationId,
};
use faktor_scm::provider::{BranchSpec, CommentTarget, PullRequestSpec, ScmProvider};
use faktor_scm::store::{MemoryScmStore, ScmStore, SqliteScmStore};
use faktor_scm::sync::ScmSync;
use faktor_scm::webhook::{
    hmac_sha256_hex, IngestOutcome, WebhookHeaders, WebhookInbox, WebhookVerifier,
};
use faktor_scm::{GitHubApp, ManualClock, StaticTokenSource};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NOW_MS: i64 = 1_700_000_000_000;

// ------------------------------------------------------------------- mock

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    /// Write the status line + headers, then abort before the body: the
    /// remote side effect happened, the response was lost (the exact crash
    /// the reconciliation protocol must survive).
    truncate: bool,
}

impl Reply {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
            truncate: false,
        }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
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

    /// Queue one reply for `METHOD path` (query strings are not part of the
    /// route key; the adapter's exact query is asserted from the recording).
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
            headers: request_headers,
            body,
        });
        state
            .routes
            .get_mut(&(method.clone(), path.clone()))
            .and_then(|queue| queue.pop_front())
    };
    let reply = reply.unwrap_or(Reply {
        status: 404,
        headers: Vec::new(),
        body: String::new(),
        truncate: false,
    });
    let reason = match reply.status {
        200 => "OK",
        201 => "Created",
        304 => "Not Modified",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        _ => "X",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let declared = if reply.truncate {
        reply.body.len() + 32
    } else {
        reply.body.len()
    };
    head.push_str(&format!(
        "Content-Length: {declared}\r\nConnection: close\r\n"
    ));
    head.push_str("\r\n");
    socket.write_all(head.as_bytes()).await?;
    if reply.truncate {
        let _ = socket.write_all(b"{").await;
        let _ = socket.shutdown().await;
        return Ok(());
    }
    socket.write_all(reply.body.as_bytes()).await?;
    let _ = query;
    Ok(())
}

// ------------------------------------------------------------------ helpers

struct Harness {
    server: MockServer,
    app: GitHubApp,
    store: Arc<dyn ScmStore>,
}

fn repository() -> RepositoryRef {
    RepositoryRef::try_new(
        ScmInstallationId::try_from_raw(7).unwrap(),
        "acme",
        "widgets",
    )
    .unwrap()
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
    Harness { server, app, store }
}

async fn harness() -> Harness {
    harness_with_store(Arc::new(MemoryScmStore::new())).await
}

fn operation(key: &str) -> ExternalOperationId {
    ExternalOperationId::try_new(key).unwrap()
}

// ------------------------------------------------------------------- tests

#[tokio::test]
async fn repository_read_uses_etag_revalidation_and_serves_304_from_cache() {
    let harness = harness().await;
    let repo = repository();
    harness.server.push(
        "GET",
        "/repos/acme/widgets",
        Reply::json(
            200,
            serde_json::json!({
                "full_name": "acme/widgets",
                "default_branch": "trunk",
                "private": true,
                "html_url": "https://example.test/acme/widgets",
            }),
        )
        .with_header("etag", "W/\"repo-1\""),
    );
    harness.server.push(
        "GET",
        "/repos/acme/widgets",
        Reply {
            status: 304,
            headers: vec![("etag".into(), "W/\"repo-1\"".into())],
            body: String::new(),
            truncate: false,
        },
    );

    let first = harness.app.repository(&repo).await.unwrap();
    assert_eq!(first.default_branch, "trunk");
    assert!(first.private);
    let second = harness.app.repository(&repo).await.unwrap();
    assert_eq!(second, first, "the 304 must be served from the ETag cache");

    let requests = harness.server.requests("GET", "/repos/acme/widgets");
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("if-none-match"), None);
    assert_eq!(
        requests[1].header("if-none-match"),
        Some("W/\"repo-1\""),
        "the revalidation must carry the cached validator"
    );
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer test-installation-token")
    );
    assert_eq!(
        requests[0].header("x-github-api-version"),
        Some("2022-11-28")
    );
}

#[tokio::test]
async fn branch_create_is_bounded_to_the_exact_head_and_reconciles_existing() {
    let harness = harness().await;
    let repo = repository();
    let head = "a".repeat(40);
    let spec = BranchSpec {
        repository: repo.clone(),
        branch: "feature/x".into(),
        head_sha: head.clone(),
        marker: "faktor:task:1:rev:1".into(),
    };

    // Attempt 1: no ref yet -> created.
    harness.server.push(
        "GET",
        "/repos/acme/widgets/git/ref/heads/feature/x",
        Reply {
            status: 404,
            headers: Vec::new(),
            body: String::new(),
            truncate: false,
        },
    );
    harness.server.push(
        "POST",
        "/repos/acme/widgets/git/refs",
        Reply::json(
            201,
            serde_json::json!({"ref": "refs/heads/feature/x", "object": {"sha": head}}),
        ),
    );
    // Attempt 2: the ref exists at the requested head -> reconciled.
    harness.server.push(
        "GET",
        "/repos/acme/widgets/git/ref/heads/feature/x",
        Reply::json(
            200,
            serde_json::json!({"ref": "refs/heads/feature/x", "object": {"sha": head}}),
        ),
    );

    let created = harness
        .app
        .create_or_reconcile_branch(&operation("op-branch-1"), &spec)
        .await
        .unwrap();
    assert!(created.created);
    assert_eq!(created.head_sha, head);
    let reconciled = harness
        .app
        .create_or_reconcile_branch(&operation("op-branch-1"), &spec)
        .await
        .unwrap();
    assert!(
        !reconciled.created,
        "an existing branch is never re-created"
    );
    assert_eq!(
        harness
            .server
            .requests("POST", "/repos/acme/widgets/git/refs")
            .len(),
        1
    );
    let row = harness
        .store
        .external_operation("op-branch-1")
        .unwrap()
        .expect("operation row");
    assert_eq!(row.state, "completed");
    assert_eq!(row.external_id.as_deref(), Some("refs/heads/feature/x"));
    assert_eq!(row.version.as_deref(), Some(head.as_str()));

    // Attempt 3: the ref drifted to another head -> typed refusal, no write.
    harness.server.push(
        "GET",
        "/repos/acme/widgets/git/ref/heads/feature/x",
        Reply::json(
            200,
            serde_json::json!({"ref": "refs/heads/feature/x", "object": {"sha": "b".repeat(40)}}),
        ),
    );
    let err = harness
        .app
        .create_or_reconcile_branch(&operation("op-branch-1"), &spec)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "scm_reconcile_conflict");
    assert_eq!(
        harness
            .server
            .requests("POST", "/repos/acme/widgets/git/refs")
            .len(),
        1
    );

    // Hostile inputs are refused before any HTTP.
    let before = harness.server.request_count();
    let mut bad = spec.clone();
    bad.head_sha = "not-a-sha".into();
    assert!(harness
        .app
        .create_or_reconcile_branch(&operation("op-branch-1"), &bad)
        .await
        .is_err());
    let mut bad_ref = spec.clone();
    bad_ref.branch = "refs/heads/x..y".into();
    assert!(harness
        .app
        .create_or_reconcile_branch(&operation("op-branch-1"), &bad_ref)
        .await
        .is_err());
    assert_eq!(
        harness.server.request_count(),
        before,
        "invalid input must never reach the network"
    );

    // The drifted attempt left the durable identity at `prepared` (the
    // remote object it observed no longer matches), never a false
    // `completed`.
    let row = harness
        .store
        .external_operation("op-branch-1")
        .unwrap()
        .expect("operation row");
    assert_eq!(row.state, "prepared");
    assert_eq!(
        row.external_id.as_deref(),
        Some("refs/heads/feature/x"),
        "a prepared rewrite must not erase the recorded remote identity"
    );
}

#[tokio::test]
async fn pull_request_reconciles_after_a_lost_create_response() {
    let harness = harness().await;
    let repo = repository();
    let marker = "faktor:task:9:rev:3";
    let spec = PullRequestSpec {
        repository: repo.clone(),
        head: "feature/x".into(),
        base: "main".into(),
        marker: marker.into(),
        title: "Faktor task 9".into(),
        body: format!("Automated by Faktor.\n\n{marker}\n"),
    };
    let pulls = "/repos/acme/widgets/pulls";

    // 1. Lookup: nothing yet.
    harness
        .server
        .push("GET", pulls, Reply::json(200, serde_json::json!([])));
    // 2. Create: the provider CREATE happened, the response was lost.
    harness.server.push(
        "POST",
        pulls,
        Reply {
            status: 201,
            headers: Default::default(),
            body: String::new(),
            truncate: true,
        },
    );
    let first = harness
        .app
        .create_or_reconcile_pull_request(&operation("op-pr-9"), &spec)
        .await;
    assert_eq!(
        first.unwrap_err().code(),
        "scm_transport",
        "the lost response must surface as a transport failure"
    );
    // The durable identity is journaled as `prepared`, not fabricated as
    // completed.
    let prepared = harness
        .store
        .external_operation("op-pr-9")
        .unwrap()
        .expect("prepared row");
    assert_eq!(prepared.state, "prepared");
    assert_eq!(prepared.marker, marker);

    // 3. The retry (same operation identity) finds the existing PR by the
    //    exact head/base/marker identity and creates NOTHING.
    harness.server.push(
        "GET",
        pulls,
        Reply::json(
            200,
            serde_json::json!([{
                "number": 42,
                "state": "open",
                "body": spec.body,
                "html_url": "https://example.test/acme/widgets/pull/42",
                "updated_at": "2024-01-02T03:04:05Z",
                "head": {"ref": "feature/x", "sha": "c".repeat(40)},
                "base": {"ref": "main"},
            }]),
        ),
    );
    let reconciled = harness
        .app
        .create_or_reconcile_pull_request(&operation("op-pr-9"), &spec)
        .await
        .unwrap();
    assert_eq!(reconciled.reference.number(), 42);
    assert!(!reconciled.created);
    assert_eq!(
        harness.server.requests("POST", pulls).len(),
        1,
        "a crash retry must never duplicate the pull request"
    );
    let created = harness.server.requests("POST", pulls);
    let sent: serde_json::Value = serde_json::from_str(&created[0].body).expect("create body");
    assert_eq!(sent["head"], "feature/x");
    assert_eq!(sent["base"], "main");
    assert!(sent["body"].as_str().expect("body").contains(marker));
    let completed = harness
        .store
        .external_operation("op-pr-9")
        .unwrap()
        .expect("completed row");
    assert_eq!(completed.state, "completed");
    assert_eq!(completed.external_id.as_deref(), Some("42"));

    // A DIFFERENT PR with the same marker is ambiguous -> typed refusal.
    harness.server.push(
        "GET",
        pulls,
        Reply::json(
            200,
            serde_json::json!([{
                "number": 42,
                "state": "open",
                "body": spec.body,
                "html_url": "u",
                "updated_at": "2024-01-02T03:04:05Z",
                "head": {"ref": "feature/x", "sha": "c".repeat(40)},
                "base": {"ref": "main"},
            }, {
                "number": 43,
                "state": "open",
                "body": spec.body,
                "html_url": "u",
                "updated_at": "2024-01-02T03:04:05Z",
                "head": {"ref": "feature/x", "sha": "d".repeat(40)},
                "base": {"ref": "main"},
            }]),
        ),
    );
    assert_eq!(
        harness
            .app
            .create_or_reconcile_pull_request(&operation("op-pr-9"), &spec)
            .await
            .unwrap_err()
            .code(),
        "scm_reconcile_conflict"
    );
}

#[tokio::test]
async fn comments_review_events_and_issue_reads_roundtrip() {
    let harness = harness().await;
    let repo = repository();
    let pr = PullRequestRef::try_new(repo.clone(), 42).unwrap();
    let issue = IssueRef::try_new(repo.clone(), 5).unwrap();

    harness.server.push(
        "POST",
        "/repos/acme/widgets/issues/42/comments",
        Reply::json(
            201,
            serde_json::json!({"id": 99, "html_url": "https://example.test/c/99"}),
        ),
    );
    harness.server.push(
        "GET",
        "/repos/acme/widgets/pulls/42/reviews",
        Reply::json(
            200,
            serde_json::json!([{
                "id": 1,
                "user": {"login": "reviewer"},
                "state": "APPROVED",
                "submitted_at": "2024-01-02T03:04:05Z",
            }]),
        ),
    );
    harness.server.push(
        "GET",
        "/repos/acme/widgets/issues/5",
        Reply::json(
            200,
            serde_json::json!({"title": "Bug", "state": "open", "body": "broken"}),
        ),
    );

    let comment = harness
        .app
        .comment(
            &operation("op-comment-1"),
            &CommentTarget::PullRequest(pr.clone()),
            "review please",
        )
        .await
        .unwrap();
    assert_eq!(comment.id, "99");
    let events = harness.app.review_events(&pr).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reviewer, "reviewer");
    assert_eq!(events[0].state, "APPROVED");
    assert_eq!(events[0].submitted_ms, Some(1_704_164_645_000));
    let read = harness.app.issue(&issue).await.unwrap();
    assert_eq!(read.title, "Bug");

    // An oversized comment is refused before any network call.
    let before = harness.server.request_count();
    assert!(harness
        .app
        .comment(
            &operation("op-comment-2"),
            &CommentTarget::Issue(issue),
            &"x".repeat(70_000),
        )
        .await
        .is_err());
    assert_eq!(harness.server.request_count(), before);
}

#[tokio::test]
async fn rate_limit_backoff_is_recorded_and_never_hot_loops() {
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let server = MockServer::start().await;
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
        clock.clone(),
    )
    .expect("adapter");
    let repo = repository();

    let reset_secs = (NOW_MS / 1000 + 120).to_string();
    server.push(
        "GET",
        "/repos/acme/widgets",
        Reply::json(
            403,
            serde_json::json!({"message": "API rate limit exceeded"}),
        )
        .with_header("x-ratelimit-remaining", "0")
        .with_header("x-ratelimit-reset", &reset_secs),
    );
    server.push(
        "GET",
        "/repos/acme/widgets",
        Reply::json(200, serde_json::json!({"full_name": "acme/widgets"})),
    );

    let err = app.repository(&repo).await.unwrap_err();
    assert_eq!(err.code(), "scm_rate_limited");
    let recorded = store
        .rate_limit("github")
        .unwrap()
        .expect("backoff must be recorded");
    assert_eq!(recorded.until_ms, (NOW_MS / 1000 + 120) * 1000 + 1_000);

    // The next call returns the recorded backoff WITHOUT touching the
    // network: no hot loop.
    let before = server.request_count();
    for _ in 0..5 {
        let err = app.repository(&repo).await.unwrap_err();
        assert_eq!(err.code(), "scm_rate_limited");
        match err {
            faktor_scm::ScmError::RateLimited { retry_after_ms } => {
                assert!(retry_after_ms > 0);
            }
            other => panic!("unexpected error {other:?}"),
        }
    }
    assert_eq!(
        server.request_count(),
        before,
        "a backed-off provider must not be called"
    );

    // Past the reset the adapter calls again and honors the refilled budget.
    clock.advance(121_000);
    let repo_read = app.repository(&repo).await.unwrap();
    assert_eq!(repo_read.full_name, "acme/widgets");
    assert_eq!(server.request_count(), before + 1);
}

#[tokio::test]
async fn installation_and_repository_sync_converges_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scm.db");
    let server = MockServer::start().await;
    server.push(
        "GET",
        "/app/installations",
        Reply::json(
            200,
            serde_json::json!([{
                "id": 7,
                "account": {"login": "acme", "type": "Organization"},
                "permissions": {"contents": "write", "metadata": "read"},
            }]),
        ),
    );
    for _ in 0..2 {
        server.push(
            "GET",
            "/installation/repositories",
            Reply::json(
                200,
                serde_json::json!({"total_count": 2, "repositories": [
                    {"name": "widgets", "owner": {"login": "acme"},
                     "full_name": "acme/widgets", "default_branch": "main",
                     "private": true, "archived": false, "html_url": "u1"},
                    {"name": "gadgets", "owner": {"login": "acme"},
                     "full_name": "acme/gadgets", "default_branch": "main",
                     "private": false, "archived": true, "html_url": "u2"},
                ]}),
            ),
        );
    }
    server.push(
        "GET",
        "/app/installations",
        Reply::json(
            200,
            serde_json::json!([{
                "id": 7,
                "account": {"login": "acme", "type": "Organization"},
                "permissions": {"contents": "write"},
            }]),
        ),
    );
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let provider = Arc::new(
        GitHubApp::new(
            GitHubAppConfig {
                api_base: server.base(),
                ..Default::default()
            },
            Arc::new(PolicyCheckedHttpTransport::permissive()),
            Arc::new(
                StaticTokenSource::minimal(NOW_MS.saturating_add(3_600_000)).expect("token source"),
            ),
            Arc::new(MemoryScmStore::new()),
            clock.clone(),
        )
        .expect("adapter"),
    );

    {
        let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&path).unwrap());
        let sync = ScmSync::new(provider.clone(), store.clone(), clock.clone());
        let first = sync.sync_all("org:alpha").await.unwrap();
        assert_eq!((first.installations, first.repositories), (1, 2));
        let second = sync.sync_all("org:alpha").await.unwrap();
        assert_eq!((second.installations, second.repositories), (1, 2));
        assert_eq!(
            store
                .repositories_for_organization("org:alpha", 0, 10)
                .unwrap()
                .len(),
            2
        );
    }
    // Restart: a fresh process reopens the same database and syncs again.
    let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&path).unwrap());
    assert_eq!(store.installations().unwrap().len(), 1);
    let sync = ScmSync::new(provider, store.clone(), clock);
    // The mock's scripted replies were consumed; new replies are needed for
    // the second process (a fresh page per call).
    server.push(
        "GET",
        "/app/installations",
        Reply::json(
            200,
            serde_json::json!([{
                "id": 7,
                "account": {"login": "acme", "type": "Organization"},
                "permissions": {"contents": "write"},
            }]),
        ),
    );
    server.push(
        "GET",
        "/installation/repositories",
        Reply::json(
            200,
            serde_json::json!({"total_count": 2, "repositories": [
                {"name": "widgets", "owner": {"login": "acme"},
                 "full_name": "acme/widgets", "default_branch": "main",
                 "private": true, "archived": false, "html_url": "u1"},
                {"name": "gadgets", "owner": {"login": "acme"},
                 "full_name": "acme/gadgets", "default_branch": "main",
                 "private": false, "archived": true, "html_url": "u2"},
            ]}),
        ),
    );
    let third = sync.sync_all("org:alpha").await.unwrap();
    assert_eq!((third.installations, third.repositories), (1, 2));
    let rows = store
        .repositories_for_organization("org:alpha", 0, 10)
        .unwrap();
    assert_eq!(rows.len(), 2, "a post-restart sync must not duplicate rows");
    assert_eq!(rows[0].full_name, "acme/widgets");
    assert_eq!(store.repositories_for_installation(7).unwrap().len(), 2);
}

#[tokio::test]
async fn webhook_dedupe_survives_a_restart_and_refuses_bad_signatures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scm.db");
    let secret = b"hook-secret".to_vec();
    let body = br#"{"action":"created","installation":{"id":7}}"#;
    let delivery = "d-0001";
    let headers = WebhookHeaders {
        signature_256: Some(format!("sha256={}", hmac_sha256_hex(&secret, body))),
        delivery_id: Some(delivery.into()),
        event: Some("installation".into()),
        timestamp_ms: None,
        timestamp_malformed: false,
    };

    {
        let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&path).unwrap());
        let inbox = WebhookInbox::new(WebhookVerifier::new(secret.clone()).unwrap(), store.clone());
        // A bad signature is refused and leaves nothing durable.
        let mut bad = headers.clone();
        bad.signature_256 = Some(format!("sha256={}", hmac_sha256_hex(b"wrong", body)));
        assert_eq!(
            inbox.ingest(&bad, body, NOW_MS).unwrap_err(),
            faktor_scm::WebhookError::SignatureMismatch
        );
        assert!(store.webhook_deliveries(10).unwrap().is_empty());
        assert!(matches!(
            inbox.ingest(&headers, body, NOW_MS).unwrap(),
            IngestOutcome::Accepted(_)
        ));
    }
    // Restart: the SAME delivery redelivered after a crash is deduped.
    let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&path).unwrap());
    let inbox = WebhookInbox::new(WebhookVerifier::new(secret).unwrap(), store.clone());
    match inbox.ingest(&headers, body, NOW_MS + 60_000).unwrap() {
        IngestOutcome::Duplicate {
            delivery_id,
            first_seen_ms,
        } => {
            assert_eq!(delivery_id, delivery);
            assert_eq!(first_seen_ms, NOW_MS, "the first-seen truth is preserved");
        }
        other => panic!("expected a duplicate, got {other:?}"),
    }
    assert_eq!(store.webhook_deliveries(10).unwrap().len(), 1);
}

#[tokio::test]
async fn missing_token_permission_refuses_before_any_call() {
    let server = MockServer::start().await;
    let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
    let app = GitHubApp::new(
        GitHubAppConfig {
            api_base: server.base(),
            ..Default::default()
        },
        Arc::new(PolicyCheckedHttpTransport::permissive()),
        Arc::new(StaticTokenSource::new(
            faktor_scm::InstallationToken::new(
                "read-only-token",
                NOW_MS + 3_600_000,
                vec![("contents".into(), "read".into())],
            )
            .unwrap(),
        )),
        store,
        Arc::new(ManualClock::new(NOW_MS)),
    )
    .expect("adapter");
    let err = app.repository(&repository()).await.unwrap_err();
    assert_eq!(err.code(), "scm_forbidden");
    assert_eq!(
        server.request_count(),
        0,
        "a token without the minimal permissions must never be used"
    );
}

#[tokio::test]
async fn remote_ref_lookup_reports_absence_without_creating() {
    let harness = harness().await;
    let reference = RemoteRef::try_new(repository(), "refs/heads/missing").unwrap();
    harness.server.push(
        "GET",
        "/repos/acme/widgets/git/ref/heads/missing",
        Reply {
            status: 404,
            headers: Vec::new(),
            body: String::new(),
            truncate: false,
        },
    );
    assert!(harness.app.remote_ref(&reference).await.unwrap().is_none());
    assert_eq!(
        harness
            .server
            .requests("GET", "/repos/acme/widgets/git/ref/heads/missing")
            .len(),
        1
    );
    assert_eq!(
        harness
            .server
            .requests("POST", "/repos/acme/widgets/git/refs")
            .len(),
        0
    );
}
