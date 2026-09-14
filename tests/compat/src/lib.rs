//! faktor-tests-compat — the compat-surface inventory suite.
//!
//! Two independent contracts are locked here:
//!
//! 1. **Route inventory** (`route_inventory.json`, this crate's root): the
//!    exact registered HTTP surface of the daemon, extracted by READING
//!    `crates/server/src/api.rs` serve() and then PROVEN live. Every
//!    registered {method, path} row is probed against an in-process server
//!    (mirror of `tests/integration`'s harness: temp-dir SessionManager +
//!    AgentRuntime over a scripted FakeProvider) WITHOUT auth. A route is
//!    proven registered when the probe answers anything but 404 (401 = the
//!    authed handler ran; 405/400/415/422 = the router matched and the
//!    framework or an extractor rejected before the handler). The observed
//!    row set must equal the checked-in golden file on every run, so
//!    accidental API drift (deleted, renamed, re-classed route) fails CI.
//!
//!    `status_class` reflects which surface in api.rs the route belongs to:
//!      - `native` — Faktor Native Protocol v1 (`/session/{id}/projection`,
//!        `/models`, `/capabilities`): the daemon's own surface.
//!      - `compat` — the frozen v7.5.6 wire-compat block plus the legacy
//!        `/api/*` aliases: optional glue, surface frozen.
//!      - `protected` — the SDK-shaped primary surface: auth-gated, 401 when
//!        probed without credentials.
//!
//!    The one public exception is the legacy `GET /api/hello` alias (200).
//!
//!    Regeneration is deliberate and two-pass: with
//!    `FAKTOR_COMPAT_REGEN_INVENTORY=1` the file is overwritten from live
//!    probes; when the file is missing the test writes it and fails once,
//!    asking for a second run to lock stability.
//!
//! 2. **Compat fixture goldens**: every JSON under `compat/kilo-v756/` (the
//!    upstream v7.5.6 wire corpus, provenance documented in
//!    `compat/README.md`) must still parse and satisfy minimal per-file-type
//!    invariants (required top-level keys, type discriminators, structural
//!    self-consistency). This is deliberately type-free: `faktor-protocol`'s
//!    own tests lock the fixtures byte-for-byte against the wire DTOs; this
//!    suite independently inventories the corpus.
//!
//! Everything lives under `#[cfg(test)]` (test-harness crate; the lib view
//! exists only so the workspace builds it).

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use faktor_agent::{
        AgentDeps, AgentRuntime, NoEvidence, PermissionRequester, RecoveryHint, Tool, ToolOutcome,
        ToolRegistry,
    };
    use faktor_core::capability::PermissionDecision;
    use faktor_core::model::ModelCapabilities;
    use faktor_core::time::SystemClock;
    use faktor_provider::{
        FakeProvider, GenericAgentRequest, Provider, ProviderError, ProviderErrorKind,
        ProviderRegistry, ProviderStream, ScriptedResponse,
    };
    use faktor_server::permission::ChannelPermissionRequester;
    use faktor_server::{serve, ServerDeps, ServerHandle, ServerPassword};
    use faktor_session::SessionManager;
    use serde_json::{json, Map, Value};
    use tempfile::tempdir;

    /// Catalog-only probe provider (family `openai`) for the SDK
    /// `provider.list` replay: its REAL `known_models` carry one documented
    /// Exact row (`gpt-4o`) and one model absent from the catalog
    /// (`mystery-proxy-model`, Unknown pricing). The catalog rows come from
    /// the provider trait's real builtin lookup; nothing ever streams.
    struct CatalogProbe;

    impl Provider for CatalogProbe {
        fn id(&self) -> &str {
            "openai"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities {
                context: 128_000,
                max_output: 16_384,
                tools: true,
                thinking: true,
                vision: true,
                json_schema: true,
                ..Default::default()
            }
        }

        fn known_models(&self) -> Vec<String> {
            vec!["gpt-4o".into(), "mystery-proxy-model".into()]
        }

        fn stream(&self, _req: GenericAgentRequest) -> ProviderStream {
            faktor_provider::provider_error_stream(ProviderError::with_code(
                ProviderErrorKind::BadRequest,
                "catalog_only",
                "the catalog probe never streams",
            ))
        }
    }

    // ------------------------------------------------------------- harness

    /// Session manager + agent + server deps over a scripted FakeProvider
    /// (mirror of `tests/integration`'s harness). The returned password is
    /// generated AFTER construction so no ambient `FAKTOR_SERVER_PASSWORD`
    /// can make the authenticated probes nondeterministic.
    pub(crate) fn server_deps(root: &Path) -> (ServerDeps, ServerPassword) {
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(Arc::new(FakeProvider::with_script(
                "fake",
                ModelCapabilities {
                    tools: true,
                    ..Default::default()
                },
                vec![ScriptedResponse::Text("pong".into()), ScriptedResponse::End],
            )))
            .unwrap();
        let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
        let permissions = ChannelPermissionRequester::new(Duration::from_secs(30));
        let agent = AgentRuntime::new(AgentDeps {
            session: session.clone(),
            providers: Arc::new(registry),
            chunk_sink: None,
            permission_requester: permissions.clone(),
            evidence: Arc::new(NoEvidence),
            tools: Arc::new(ToolRegistry::new()),
            cas: None,
            workspaces: faktor_fs::WorkspaceFileService::new(),
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            verification: faktor_agent::VerificationService::disabled(),
            model: "m".into(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions: "You are a test server agent.".into(),
            hooks: None,
            instructions_resolver: faktor_instructions::no_roots_resolver(),
            routing: faktor_agent::FixedRoutingPolicy::passthrough(),
            budgets: Arc::new(faktor_session::NoopBudget),
            clock: Arc::new(SystemClock),
            tool_call_mode: faktor_agent::ToolCallMode::Native,
            tool_deadline_ms: 2000,
            retry_policy: faktor_core::retry::RetryPolicy::default(),
            semantic: faktor_agent::fallback_semantic_registry(),
            context_prior: None,
            efficiency: Default::default(),
        })
        .unwrap();
        let mut deps = ServerDeps::new(session, agent, permissions);
        let password = ServerPassword::generate();
        deps.server_password = password.clone();
        (deps, password)
    }

    pub(crate) async fn spawn_server(deps: ServerDeps) -> (ServerHandle, String) {
        let handle = serve(deps, 0).await.unwrap();
        let base = format!("http://{}", handle.addr);
        (handle, base)
    }

    // ------------------------------------------------ replay harness (snapshots)

    /// Deterministic scratch path of the checkpoint probe (relative to the
    /// wire-created session's workspace root "." — inside the crate's
    /// gitignored `target/`), namespaced by PROCESS so two concurrent test
    /// processes in the same checkout can never race the same file. The
    /// golden normalizes this path to `@string` via the driver's
    /// `probePath` var (env `FAKTOR_COMPAT_PROBE_PATH`). Removed before
    /// every replay so the probe's checkpoint is always a REAL
    /// missing→existing file transition.
    pub(crate) fn probe_path() -> String {
        format!("target/compat-replay-{}/probe.txt", std::process::id())
    }
    /// The probe's deterministic content (the recorded diff goldens derive
    /// their additions/deletions counts from it).
    pub(crate) const PROBE_CONTENT: &str = "compat probe\n";

    /// Permission requester of the replay harness ONLY: the checkpoint probe
    /// is a test fixture, so its permission hop is deterministically
    /// Allowed (mirrors `tests/integration`'s `AlwaysAllow`). Every other
    /// surface keeps the real HTTP requester (`ServerDeps.permissions`), so
    /// the permission endpoints still answer real pending views.
    #[derive(Clone)]
    struct AllowAllRequester;

    impl PermissionRequester for AllowAllRequester {
        fn request(
            &self,
            _session: faktor_core::id::SessionId,
            _permission: &faktor_session::ops::PermissionRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
        > {
            Box::pin(async { Ok(PermissionDecision::Allow) })
        }
    }

    /// A checkpoint-recording write tool (the harness fixture that makes the
    /// real revert/unrevert/diff state exist): it writes the file through
    /// the workspace handle and records the before/after pair in the ONE
    /// CAS-backed checkpoint store — exactly the shape the daemon's
    /// `write_file` records (missing file -> an existence-bearing Added row
    /// via `record_change`). No fabricated state: the CAS blobs are the
    /// bytes actually written.
    fn checkpoint_probe_tool() -> Tool {
        Tool {
            name: "checkpoint_probe".into(),
            description: "compat replay checkpoint probe".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"},
                },
                "required": ["path", "content"],
            }),
            resource_class: faktor_core::resource::ResourceClass::DiskWrite,
            capability: None,
            recovery_hint: RecoveryHint::WorkspaceWrite,
            path_args: vec!["path".into()],
            execute: Arc::new(|ctx, args| {
                Box::pin(async move {
                    let Some(ws) = &ctx.workspace else {
                        return Err(faktor_core::error::Error::internal("no workspace wired"));
                    };
                    let Some(snaps) = &ctx.snapshots else {
                        return Err(faktor_core::error::Error::internal(
                            "no checkpoint store wired",
                        ));
                    };
                    let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    let content = args
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default();
                    let rel = std::path::Path::new(path);
                    if let Some(parent) = rel.parent() {
                        if !parent.as_os_str().is_empty() {
                            if let Ok(resolved) = ws.resolve(parent) {
                                let _ = std::fs::create_dir_all(&resolved);
                            }
                        }
                    }
                    let current = ws.read(rel, 16 * 1024 * 1024).ok();
                    match current {
                        Some(data) => {
                            if data.bytes == content.as_bytes() {
                                return Ok(ToolOutcome {
                                    text: format!("{path} unchanged"),
                                    exit_code: Some(0),
                                    ..Default::default()
                                });
                            }
                            let before = snaps.before_write(ctx.session_id, path, &data.bytes)?;
                            let after = ws.write_atomic(rel, content.as_bytes()).map_err(|e| {
                                faktor_core::error::Error::internal(format!("write {path}: {e}"))
                            })?;
                            snaps.after_write(
                                ctx.session_id,
                                path,
                                before,
                                after,
                                0,
                                content.as_bytes(),
                            )?;
                        }
                        None => {
                            let after = ws.write_atomic(rel, content.as_bytes()).map_err(|e| {
                                faktor_core::error::Error::internal(format!("write {path}: {e}"))
                            })?;
                            if let Err(e) = snaps.record_change(
                                ctx.session_id,
                                path,
                                faktor_snapshot::FileState::missing(),
                                None,
                                faktor_snapshot::FileState::existing(after),
                                Some(content.as_bytes()),
                            ) {
                                return Err(faktor_core::error::Error::internal(format!(
                                    "checkpoint {path}: {e}"
                                )));
                            }
                        }
                    }
                    Ok(ToolOutcome {
                        text: "checkpoint probe recorded".to_string(),
                        exit_code: Some(0),
                        ..Default::default()
                    })
                })
            }),
        }
    }

    /// The replay's daemon deps: the SAME store/agent/server graph as
    /// [`server_deps`], plus the REAL CAS-backed checkpoint store and the
    /// checkpoint probe tool, so the replay exercises the wire revert /
    /// unrevert / diff routes against durable state instead of refusing
    /// with "snapshots unavailable". The probe scratch file is removed
    /// before the run (test scratch, never durable state). Returns the
    /// per-process probe path too, so the caller can hand it to the driver
    /// as `FAKTOR_COMPAT_PROBE_PATH` (golden-normalized to `@string`).
    pub(crate) fn replay_server_deps(root: &Path) -> (ServerDeps, ServerPassword, String) {
        let probe_path = probe_path();
        let scratch = std::path::Path::new(&probe_path);
        if let Some(parent) = scratch.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&probe_path);
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(Arc::new(FakeProvider::with_script(
                "fake",
                ModelCapabilities {
                    tools: true,
                    ..Default::default()
                },
                vec![
                    ScriptedResponse::ToolCall {
                        id: "compat-probe-1".into(),
                        name: "checkpoint_probe".into(),
                        input: json!({"path": probe_path, "content": PROBE_CONTENT}),
                    },
                    ScriptedResponse::Text("pong".into()),
                    ScriptedResponse::End,
                ],
            )))
            .unwrap();
        // The SDK `provider.list` projection is exercised over a REAL
        // catalog provider: one documented Exact row and one Unknown model.
        registry.try_register(Arc::new(CatalogProbe)).unwrap();
        let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
        let fs = faktor_fs::WorkspaceFileService::new();
        let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(
            session.cas(),
            session.store(),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(checkpoint_probe_tool());
        let agent = AgentRuntime::new(AgentDeps {
            session: session.clone(),
            providers: Arc::new(registry),
            chunk_sink: None,
            permission_requester: Arc::new(AllowAllRequester),
            evidence: Arc::new(NoEvidence),
            tools: Arc::new(tools),
            cas: Some(session.cas()),
            workspaces: fs.clone(),
            edit: None,
            snapshots: Some(snapshots.clone()),
            sandbox: None,
            supervisor: None,
            verification: faktor_agent::VerificationService::disabled(),
            model: "m".into(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions: "You are a test server agent.".into(),
            hooks: None,
            instructions_resolver: faktor_instructions::no_roots_resolver(),
            routing: faktor_agent::FixedRoutingPolicy::passthrough(),
            budgets: Arc::new(faktor_session::NoopBudget),
            clock: Arc::new(SystemClock),
            tool_call_mode: faktor_agent::ToolCallMode::Native,
            tool_deadline_ms: 2000,
            retry_policy: faktor_core::retry::RetryPolicy::default(),
            semantic: faktor_agent::fallback_semantic_registry(),
            context_prior: None,
            efficiency: Default::default(),
        })
        .unwrap();
        let permissions = ChannelPermissionRequester::new(Duration::from_secs(30));
        let mut deps = ServerDeps::new(session, agent, permissions).with_snapshots(fs, snapshots);
        let password = ServerPassword::generate();
        deps.server_password = password.clone();
        (deps, password, probe_path)
    }

    /// One HTTP probe. POSTs carry an empty JSON object (enough to pass
    /// optional-bodied extractors; required fields fail extraction with
    /// 400/415/422 — never 404). Unauthenticated requests cannot reach any
    /// handler effect: every handler except the public hello alias gates on
    /// auth first.
    async fn probe(client: &reqwest::Client, base: &str, verb: &str, path: &str) -> u16 {
        let builder = match verb {
            "GET" => client.get(format!("{base}{path}")),
            "POST" => client
                .post(format!("{base}{path}"))
                .header("content-type", "application/json")
                .json(&json!({})),
            "PATCH" => client
                .patch(format!("{base}{path}"))
                .header("content-type", "application/json")
                .json(&json!({})),
            "DELETE" => client.delete(format!("{base}{path}")),
            other => panic!("unsupported probe verb {other}"),
        };
        let resp = tokio::time::timeout(Duration::from_secs(20), builder.send())
            .await
            .unwrap_or_else(|_| panic!("probe {verb} {path} timed out"))
            .unwrap_or_else(|e| panic!("probe {verb} {path} failed: {e}"));
        resp.status().as_u16()
    }

    async fn authed_get_json(
        client: &reqwest::Client,
        base: &str,
        password: &ServerPassword,
        path: &str,
    ) -> Value {
        let resp = client
            .get(format!("{base}{path}"))
            .bearer_auth(password.as_str())
            .send()
            .await
            .unwrap_or_else(|e| panic!("authed GET {path} failed: {e}"));
        assert_eq!(
            resp.status(),
            200,
            "authed GET {path} must be 200, got {}",
            resp.status()
        );
        resp.json()
            .await
            .unwrap_or_else(|e| panic!("GET {path} body not JSON: {e}"))
    }

    // ---------------------------------------------------- route inventory

    /// Every {method, path} registration in `crates/server/src/api.rs`
    /// serve() (read line by line from the router builder), tagged with the
    /// api.rs surface section it belongs to:
    ///   compat   = legacy `/api/*` aliases + the v7.5.6 wire-compat block;
    ///   protected= the SDK-shaped primary surface;
    ///   native   = Faktor Native Protocol v1.
    const ROUTES: &[(&str, &str, &str)] = &[
        // Legacy aliases (api.rs: "Legacy aliases (frozen for old tests)").
        ("GET", "/api/hello", "compat"),
        ("POST", "/api/session", "compat"),
        ("GET", "/api/sessions", "compat"),
        ("GET", "/api/session/{id}", "compat"),
        ("GET", "/api/session/{id}/state", "compat"),
        ("GET", "/api/session/{id}/messages", "compat"),
        ("GET", "/api/session/{id}/events", "compat"),
        ("POST", "/api/session/{id}/prompt", "compat"),
        ("POST", "/api/session/{id}/abort", "compat"),
        ("POST", "/api/perm/{id}/resolve", "compat"),
        ("GET", "/api/provider", "compat"),
        // SDK-shaped primary surface.
        ("POST", "/session/create", "protected"),
        ("POST", "/session/prompt", "protected"),
        ("POST", "/session/abort", "protected"),
        ("GET", "/session/messages", "protected"),
        ("GET", "/session/state", "protected"),
        ("GET", "/session/list", "protected"),
        ("POST", "/permission/reply", "protected"),
        ("GET", "/permission/list", "protected"),
        ("GET", "/provider/list", "protected"),
        ("GET", "/global/health", "protected"),
        ("GET", "/global/event", "protected"),
        ("POST", "/question/reply", "protected"),
        ("GET", "/question/list", "protected"),
        ("POST", "/network/reply", "protected"),
        ("GET", "/network/list", "protected"),
        ("GET", "/config/get", "protected"),
        ("POST", "/config/set", "protected"),
        // SDK-exact aliases (the unmodified `@kilocode/sdk@7.5.6` paths).
        ("GET", "/config", "compat"),
        ("GET", "/permission", "compat"),
        ("GET", "/question", "compat"),
        ("GET", "/network", "compat"),
        ("GET", "/provider", "compat"),
        // v7.5.6 wire-compat surface (subset the frozen extension calls).
        ("POST", "/session", "compat"),
        ("GET", "/session", "compat"),
        ("GET", "/session/{sessionID}", "compat"),
        ("POST", "/session/{sessionID}", "compat"),
        ("PATCH", "/session/{sessionID}", "compat"),
        ("DELETE", "/session/{sessionID}", "compat"),
        ("POST", "/session/{sessionID}/fork", "compat"),
        ("POST", "/session/{sessionID}/summarize", "compat"),
        ("POST", "/session/{sessionID}/message", "compat"),
        ("GET", "/session/{sessionID}/message", "compat"),
        (
            "DELETE",
            "/session/{sessionID}/message/{messageID}",
            "compat",
        ),
        ("POST", "/session/{sessionID}/abort", "compat"),
        ("GET", "/session/{sessionID}/diff", "compat"),
        ("POST", "/session/{sessionID}/revert", "compat"),
        ("POST", "/session/{sessionID}/unrevert", "compat"),
        ("GET", "/session/{sessionID}/state", "compat"),
        ("GET", "/session/{sessionID}/status", "compat"),
        ("GET", "/session/status", "compat"),
        ("POST", "/question/reject", "compat"),
        ("POST", "/network/reject", "compat"),
        ("POST", "/config/update", "compat"),
        ("GET", "/config/warnings", "compat"),
        ("GET", "/config/overlay", "compat"),
        ("POST", "/config/overlay", "compat"),
        ("POST", "/config/overlayUpdate", "compat"),
        ("POST", "/pty/create", "compat"),
        ("POST", "/pty/update", "compat"),
        ("POST", "/pty/remove", "compat"),
        ("POST", "/pty", "compat"),
        ("DELETE", "/pty/{ptyID}", "compat"),
        ("GET", "/pty/{pty_id}/output", "compat"),
        ("POST", "/global/dispose", "compat"),
        ("POST", "/instance/dispose", "compat"),
        ("POST", "/instance/reload", "compat"),
        ("POST", "/auth/set", "compat"),
        ("POST", "/auth/remove", "compat"),
        // Faktor Native Protocol v1.
        ("GET", "/session/{id}/projection", "native"),
        ("GET", "/models", "native"),
        ("GET", "/capabilities", "native"),
    ];

    /// Statuses that prove registration without credentials: 401 = the
    /// authed handler ran; 405 = the path matched, the verb did not;
    /// 400/415/422 = an extractor rejected before the handler. Anything
    /// else (404 included) is drift.
    const PROVEN: &[u16] = &[401, 405, 400, 415, 422];

    fn golden_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("route_inventory.json")
    }

    fn sort_rows(rows: Vec<Value>) -> Vec<Value> {
        let mut rows = rows;
        rows.sort_by(|a, b| {
            let ka = format!(
                "{} {} {}",
                a["method"].as_str().unwrap_or(""),
                a["path"].as_str().unwrap_or(""),
                a["status_class"].as_str().unwrap_or("")
            );
            let kb = format!(
                "{} {} {}",
                b["method"].as_str().unwrap_or(""),
                b["path"].as_str().unwrap_or(""),
                b["status_class"].as_str().unwrap_or("")
            );
            ka.cmp(&kb)
        });
        rows
    }

    /// Probe every registered route unauthenticated; the observed row set
    /// must equal the checked-in golden (two-pass: missing golden is
    /// written and the test fails once; `FAKTOR_COMPAT_REGEN_INVENTORY=1`
    /// rewrites deliberately).
    #[tokio::test]
    async fn route_inventory_matches_golden() {
        let dir = tempdir().unwrap();
        let (deps, _password) = server_deps(dir.path());
        let (handle, base) = spawn_server(deps).await;
        let client = reqwest::Client::new();

        let mut rows: Vec<Value> = Vec::new();
        let mut reached_handler = 0usize;
        for &(verb, path, class) in ROUTES {
            let status = probe(&client, &base, verb, path).await;
            if path == "/api/hello" {
                // The one public legacy alias: 200 without credentials.
                assert_eq!(
                    status, 200,
                    "GET /api/hello is the documented public alias, got {status}"
                );
            } else {
                assert!(
                    PROVEN.contains(&status),
                    "route {verb} {path} ({class}) unproven: unauthenticated probe answered \
                     {status} — expected 401 (authed handler) or 405/400/415/422 (matched, \
                     rejected before the handler); 404 means the route is NOT registered"
                );
            }
            if status == 401 {
                reached_handler += 1;
            }
            rows.push(json!({ "method": verb, "path": path, "status_class": class }));
        }
        // Sanity: the bulk of the surface must actually reach its authed
        // handler (a shadowing route or a mass extractor change that turned
        // everything into early 4xx would silently weaken the proof).
        // Floor: the bulk of the surface must reach its authed handler (the
        // rest is extractor rejection before the handler — 400/415/422 — or
        // the one public alias). A mass auth removal or a shadowing route
        // would push every row away from 401 and trip this.
        assert!(
            reached_handler >= 30,
            "only {reached_handler}/{} rows reached an authed handler; auth removed or              routing shadowing?",
            ROUTES.len()
        );

        let rows = sort_rows(rows);
        let expected = load_or_regenerate_golden(&rows);
        assert_eq!(
            serde_json::to_string_pretty(&rows).unwrap(),
            serde_json::to_string_pretty(&expected).unwrap(),
            "route inventory drift: the live surface differs from {} — regenerate only on \
             deliberate API change (FAKTOR_COMPAT_REGEN_INVENTORY=1), then re-run",
            golden_path().display()
        );
        let _ = handle.shutdown.send(());
    }

    fn load_or_regenerate_golden(observed: &[Value]) -> Vec<Value> {
        let path = golden_path();
        let regen = std::env::var("FAKTOR_COMPAT_REGEN_INVENTORY")
            .map(|v| v == "1")
            .unwrap_or(false);
        if regen {
            write_golden(&path, observed);
            eprintln!(
                "route inventory regenerated from live probes at {}",
                path.display()
            );
            return observed.to_vec();
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("route inventory {} is corrupt: {e}", path.display())),
            Err(_) => {
                write_golden(&path, observed);
                panic!(
                    "route inventory {} did not exist — wrote it from live probes; re-run the \
                     test once so the golden is compared against a second observation",
                    path.display()
                );
            }
        }
    }

    fn write_golden(path: &Path, rows: &[Value]) {
        let text = format!("{}\n", serde_json::to_string_pretty(&rows).unwrap());
        std::fs::write(path, text)
            .unwrap_or_else(|e| panic!("cannot write route inventory {}: {e}", path.display()));
    }

    // --------------------------------------------------- native surface

    /// The native-surface contract: `/global/health` is 200 with auth (and
    /// 401 without), `/session/{id}/projection` is 401 without auth, and
    /// after one real FakeProvider-driven session+prompt the projection is
    /// 200 and carries the documented shape with the state present.
    #[tokio::test]
    async fn native_surface_probe() {
        let dir = tempdir().unwrap();
        let (deps, password) = server_deps(dir.path());
        let (handle, base) = spawn_server(deps).await;
        let client = reqwest::Client::new();

        // Health: auth-required (frozen client authenticates everything).
        let unauth = probe(&client, &base, "GET", "/global/health").await;
        assert_eq!(
            unauth, 401,
            "/global/health without credentials must be 401"
        );
        let body = authed_get_json(&client, &base, &password, "/global/health").await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["protocol"], "v756");
        assert!(body["version"].as_str().is_some_and(|v| !v.is_empty()));

        // Projection: auth-gated before the session id is even consulted.
        let unauth = probe(&client, &base, "GET", "/session/does-not-exist/projection").await;
        assert_eq!(
            unauth, 401,
            "/session/{{id}}/projection without credentials must be 401"
        );

        // One real session + prompt over the legacy surface.
        let resp = client
            .post(format!("{base}/api/session"))
            .bearer_auth(password.as_str())
            .json(&json!({ "provider": "fake", "model": "m", "title": "native probe" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let created: Value = resp.json().await.unwrap();
        let sid = created["id"].as_str().unwrap().to_string();

        let resp = client
            .post(format!("{base}/api/session/{sid}/prompt"))
            .bearer_auth(password.as_str())
            .json(&json!({ "prompt": "hi" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Poll the native projection until the scripted turn finishes.
        let mut projection = Value::Null;
        for _ in 0..250 {
            projection = authed_get_json(
                &client,
                &base,
                &password,
                &format!("/session/{sid}/projection"),
            )
            .await;
            if projection["state"]["active"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            projection["state"]["machine"], "ready_for_next_turn",
            "scripted turn must reach ready_for_next_turn"
        );

        // Documented native projection shape (api.rs build_native_projection).
        let keys: Vec<&str> = projection
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        for required in [
            "session",
            "state",
            "activeModel",
            "activeTool",
            "progress",
            "filesChanged",
            "lastCheckpoint",
            "verification",
            "contextUsage",
            "queued",
        ] {
            assert!(
                keys.contains(&required),
                "projection missing key {required}: {projection}"
            );
        }
        assert_eq!(projection["session"]["id"], sid);
        assert_eq!(projection["session"]["provider"], "fake");
        let state = projection["state"].as_object().unwrap();
        for required in ["machine", "label", "active", "terminal"] {
            assert!(
                state.contains_key(required),
                "projection state missing key {required}"
            );
        }
        // After a finished turn the effective model envelope is durable.
        assert_eq!(projection["activeModel"]["provider"], "fake");
        assert_eq!(projection["activeModel"]["model"], "m");
        let _ = handle.shutdown.send(());
    }

    // -------------------------------------------- compat fixture goldens

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/kilo-v756")
    }

    fn fixture_entries() -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(fixture_root())
            .unwrap_or_else(|e| {
                panic!(
                    "compat fixture dir {} missing (upstream-corpus provenance): {e}",
                    fixture_root().display()
                )
            })
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect();
        entries.sort();
        entries
    }

    fn load_fixture(path: &Path) -> Value {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("fixture {} unreadable: {e}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("fixture {} is not valid JSON: {e}", path.display()))
    }

    fn object<'a>(v: &'a Value, file: &str, what: &str) -> &'a Map<String, Value> {
        v.as_object()
            .unwrap_or_else(|| panic!("fixture {file}: {what} must be an object, got {v}"))
    }

    /// Required key lookup against a JSON object; panics with context.
    fn key<'a>(o: &'a Map<String, Value>, file: &str, what: &str) -> &'a Value {
        o.get(what)
            .unwrap_or_else(|| panic!("fixture {file}: missing required key {what:?} in {o:?}"))
    }

    /// Required non-empty string field.
    fn str_key<'a>(o: &'a Map<String, Value>, file: &str, what: &str) -> &'a str {
        let s = key(o, file, what)
            .as_str()
            .unwrap_or_else(|| panic!("fixture {file}: {what} must be a string"));
        assert!(!s.is_empty(), "fixture {file}: {what} must not be empty");
        s
    }

    /// Required array field.
    fn arr_key<'a>(o: &'a Map<String, Value>, file: &str, what: &str) -> &'a Vec<Value> {
        key(o, file, what)
            .as_array()
            .unwrap_or_else(|| panic!("fixture {file}: {what} must be an array"))
    }

    /// Required integer field.
    fn int_key(o: &Map<String, Value>, file: &str, what: &str) -> u64 {
        key(o, file, what)
            .as_u64()
            .unwrap_or_else(|| panic!("fixture {file}: {what} must be a u64"))
    }

    /// Required bool field.
    fn bool_key(o: &Map<String, Value>, file: &str, what: &str) -> bool {
        key(o, file, what)
            .as_bool()
            .unwrap_or_else(|| panic!("fixture {file}: {what} must be a bool"))
    }

    /// The fixture directory exists, is documented as upstream-compat
    /// provenance, and every checked-in corpus file is present.
    #[test]
    fn compat_fixture_dir_is_documented_and_complete() {
        let entries = fixture_entries();
        let files: Vec<String> = entries
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        for expected in [
            "basic_auth.json",
            "create_session.json",
            "errors.json",
            "global_event.json",
            "hello.json",
            "messages_page.json",
            "password_auth.json",
            "provider_list.json",
            "sdk-global-frames.json",
            "sse_frames.json",
            "startup_line.json",
            "upstream.json",
            "wire_message_send.json",
            "wire_part_union.json",
            "wire_session_create.json",
        ] {
            assert!(
                files.contains(&expected.to_string()),
                "compat corpus missing checked-in fixture {expected}"
            );
        }

        // Upstream-compat provenance is documented next to the corpus.
        let readme = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compat/README.md");
        let doc = std::fs::read_to_string(&readme).unwrap_or_else(|e| {
            panic!("compat provenance doc {} unreadable: {e}", readme.display())
        });
        for needle in ["kilo-v756", "v7.5.6"] {
            assert!(
                doc.contains(needle),
                "compat provenance doc must mention {needle:?}"
            );
        }
        assert!(
            doc.to_lowercase().contains("golden"),
            "compat provenance doc must document the golden-test provenance"
        );
    }

    /// Every JSON under `compat/kilo-v756/` parses and satisfies the minimal
    /// per-file-type invariants (required top-level keys, type/kind
    /// discriminators, structural self-consistency). Unknown new files fail
    /// loudly until typed invariants are added — nothing rots silently.
    #[test]
    fn compat_fixtures_parse_with_minimal_invariants() {
        for path in fixture_entries() {
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            let raw = load_fixture(&path);
            match file.as_str() {
                "startup_line.json" => {
                    let o = object(&raw, &file, "root");
                    let template = str_key(o, &file, "template");
                    assert!(
                        template.contains("{port}"),
                        "fixture {file}: template must document the port slot"
                    );
                    str_key(o, &file, "example");
                }
                "hello.json" => {
                    let o = object(&raw, &file, "root");
                    assert_eq!(key(o, &file, "ok"), &json!(true));
                    assert_eq!(
                        str_key(o, &file, "protocol"),
                        "v756",
                        "fixture {file}: protocol discriminator drifted"
                    );
                    let providers = arr_key(o, &file, "providers");
                    assert!(!providers.is_empty(), "fixture {file}: providers empty");
                }
                "create_session.json" => {
                    let o = object(&raw, &file, "root");
                    for side in ["request", "response"] {
                        let obj = object(key(o, &file, side), &file, side);
                        if side == "request" {
                            str_key(obj, &file, "provider");
                        } else {
                            str_key(obj, &file, "id");
                            int_key(obj, &file, "created_ms");
                        }
                    }
                }
                "messages_page.json" => {
                    let o = object(&raw, &file, "root");
                    str_key(o, &file, "session_id");
                    bool_key(o, &file, "has_more");
                    let messages = arr_key(o, &file, "messages");
                    assert!(!messages.is_empty(), "fixture {file}: messages empty");
                    for (i, msg) in messages.iter().enumerate() {
                        let m = object(msg, &file, &format!("messages[{i}]"));
                        str_key(m, &file, "id");
                        let parts = arr_key(m, &file, "parts");
                        assert!(
                            !parts.is_empty(),
                            "fixture {file}: messages[{i}].parts empty"
                        );
                        for part in parts {
                            let p = object(part, &file, "part");
                            str_key(p, &file, "type");
                        }
                    }
                }
                "sse_frames.json" => {
                    let frames = raw.as_array().unwrap();
                    assert!(
                        frames.len() >= 5,
                        "fixture {file}: resume-cursor sequence must be complete"
                    );
                    let mut prev = 0u64;
                    for (i, f) in frames.iter().enumerate() {
                        let o = object(f, &file, &format!("frames[{i}]"));
                        let id = int_key(o, &file, "id");
                        assert!(id > prev, "fixture {file}: frame ids not monotonic");
                        prev = id;
                        let frame = str_key(o, &file, "frame");
                        assert!(
                            frame.contains(&format!("id: {id}")),
                            "fixture {file}: frame {id} must carry its own id"
                        );
                    }
                }
                "errors.json" => {
                    let cases = raw.as_array().unwrap();
                    assert!(!cases.is_empty(), "fixture {file}: no error cases");
                    for (i, case) in cases.iter().enumerate() {
                        let o = object(case, &file, &format!("cases[{i}]"));
                        str_key(o, &file, "kind");
                        str_key(o, &file, "code");
                        let status = int_key(o, &file, "http_status");
                        assert!(
                            (400..=599).contains(&status),
                            "fixture {file}: cases[{i}] status {status} not an error code"
                        );
                        bool_key(o, &file, "retryable");
                        object(key(o, &file, "body"), &file, "body");
                    }
                }
                "provider_list.json" => {
                    let o = object(&raw, &file, "root");
                    let providers = arr_key(o, &file, "providers");
                    assert!(!providers.is_empty(), "fixture {file}: providers empty");
                    for p in providers {
                        let p = object(p, &file, "provider");
                        str_key(p, &file, "kind");
                        let models = arr_key(p, &file, "models");
                        assert!(!models.is_empty(), "fixture {file}: provider has no models");
                        for m in models {
                            let m = object(m, &file, "model");
                            str_key(m, &file, "id");
                            object(key(m, &file, "capabilities"), &file, "capabilities");
                        }
                    }
                }
                "global_event.json" => {
                    let cases = raw.as_array().unwrap();
                    assert!(!cases.is_empty(), "fixture {file}: no event examples");
                    let mut names = Vec::new();
                    for (i, case) in cases.iter().enumerate() {
                        let o = object(case, &file, &format!("cases[{i}]"));
                        let name = str_key(o, &file, "name");
                        names.push(name.to_string());
                        let event = object(key(o, &file, "event"), &file, "event");
                        for k in ["directory", "project", "workspace", "payload"] {
                            key(event, &file, k);
                        }
                        let payload = object(key(event, &file, "payload"), &file, "payload");
                        assert_eq!(
                            str_key(payload, &file, "type"),
                            name,
                            "fixture {file}: payload type tag must match the example name"
                        );
                    }
                    for required in [
                        "session_created",
                        "message_part_updated",
                        "session_next_text_delta",
                    ] {
                        assert!(
                            names.iter().any(|n| n == required),
                            "fixture {file}: missing documented example {required}"
                        );
                    }
                }
                "password_auth.json" => {
                    let o = object(&raw, &file, "root");
                    assert_eq!(str_key(o, &file, "env_var"), "FAKTOR_SERVER_PASSWORD");
                    let unauthorized = object(key(o, &file, "unauthorized"), &file, "unauthorized");
                    assert_eq!(str_key(unauthorized, &file, "code"), "unauthorized");
                    assert_eq!(int_key(unauthorized, &file, "http_status"), 401);
                    assert_eq!(str_key(o, &file, "legacy_public_alias"), "/api/hello");
                }
                "basic_auth.json" => {
                    let o = object(&raw, &file, "root");
                    assert_eq!(str_key(o, &file, "env_var"), "FAKTOR_SERVER_PASSWORD");
                    assert_eq!(str_key(o, &file, "username"), "kilo");
                    assert_eq!(str_key(o, &file, "basic_scheme"), "Basic");
                    let pw = str_key(o, &file, "example_password");
                    assert_eq!(
                        pw.len(),
                        64,
                        "fixture {file}: example_password must be 64 hex"
                    );
                    str_key(o, &file, "example_header");
                }
                "wire_session_create.json" => {
                    let o = object(&raw, &file, "root");
                    let req = object(key(o, &file, "request"), &file, "request");
                    for k in ["parentID", "model", "workspaceID"] {
                        key(req, &file, k);
                    }
                    assert!(
                        !req.contains_key("parent_id"),
                        "fixture {file}: wire names are camelCase, never snake_case"
                    );
                    let resp = object(key(o, &file, "response"), &file, "response");
                    for k in ["sessionID", "title", "createdMs"] {
                        key(resp, &file, k);
                    }
                }
                "wire_message_send.json" => {
                    let o = object(&raw, &file, "root");
                    let req = object(key(o, &file, "request"), &file, "request");
                    let parts = arr_key(req, &file, "parts");
                    assert!(!parts.is_empty(), "fixture {file}: request parts empty");
                    for part in parts {
                        let p = object(part, &file, "part");
                        str_key(p, &file, "type");
                    }
                    let resp = object(key(o, &file, "response"), &file, "response");
                    let info = object(key(resp, &file, "info"), &file, "info");
                    str_key(info, &file, "sessionID");
                    arr_key(resp, &file, "parts");
                }
                "wire_part_union.json" => {
                    let cases = raw.as_array().unwrap();
                    assert!(!cases.is_empty(), "fixture {file}: no part examples");
                    for (i, case) in cases.iter().enumerate() {
                        let o = object(case, &file, &format!("cases[{i}]"));
                        let name = str_key(o, &file, "name");
                        let part = object(key(o, &file, "part"), &file, "part");
                        assert_eq!(
                            str_key(part, &file, "type"),
                            name,
                            "fixture {file}: part type tag must match the example name"
                        );
                    }
                }
                "upstream.json" => {
                    // Vendored upstream-client pin: repository/tag/commit +
                    // per-file blake3 hashes + license, regenerated only by
                    // `FAKTOR_COMPAT_REGEN_SDK_MANIFEST=1` (see upstream.rs).
                    let o = object(&raw, &file, "root");
                    assert_eq!(
                        str_key(o, &file, "repository"),
                        "https://github.com/Kilo-Org/kilocode"
                    );
                    assert_eq!(str_key(o, &file, "tag"), "v7.5.6");
                    assert_eq!(
                        str_key(o, &file, "commit"),
                        "fa02955bfa17b60e57e0d7406d200a73337472ee"
                    );
                    assert_eq!(str_key(o, &file, "hashAlgorithm"), "blake3");
                    let paths = object(key(o, &file, "paths"), &file, "paths");
                    assert!(
                        paths.contains_key("packages/sdk/js"),
                        "fixture {file}: the upstream client path must stay pinned"
                    );
                    let count = int_key(o, &file, "fileCount");
                    assert!(count > 0, "fixture {file}: empty vendored tree");
                    assert!(int_key(o, &file, "totalBytes") > 0);
                    let hashes = object(key(o, &file, "file_hashes"), &file, "file_hashes");
                    assert_eq!(
                        hashes.len() as u64,
                        count,
                        "fixture {file}: fileCount must equal the hash set size"
                    );
                    let licenses = object(key(o, &file, "licenses"), &file, "licenses");
                    assert_eq!(
                        str_key(licenses, &file, "package"),
                        "MIT",
                        "fixture {file}: license provenance drifted"
                    );
                }
                "sdk-global-frames.json" => {
                    // The SDK `global.event` union frame corpus: frames
                    // captured from the daemon's real projector, pinned to
                    // the vendored SDK's declared event-type literals.
                    // Regenerated only by the projector test
                    // (`FAKTOR_COMPAT_REGEN_GLOBAL_FRAMES=1`).
                    let o = object(&raw, &file, "root");
                    assert_eq!(
                        str_key(o, &file, "schema"),
                        "faktor.compat.sdk-global-frames/v1"
                    );
                    let sdk = object(key(o, &file, "sdk"), &file, "sdk");
                    assert_eq!(str_key(sdk, &file, "version"), "7.5.6");
                    let union = arr_key(o, &file, "union_types");
                    assert!(union.len() > 100, "fixture {file}: thin union surface");
                    let declared: Vec<&str> = union
                        .iter()
                        .map(|v| {
                            let s = v.as_str().unwrap_or_else(|| {
                                panic!("fixture {file}: union type must be a string")
                            });
                            assert!(
                                s.contains('.'),
                                "fixture {file}: union type {s:?} is not a dotted event literal"
                            );
                            s
                        })
                        .collect();
                    let scenarios = arr_key(o, &file, "scenarios");
                    assert!(!scenarios.is_empty(), "fixture {file}: no scenarios");
                    for scenario in scenarios {
                        let scenario = object(scenario, &file, "scenario");
                        assert!(
                            !str_key(scenario, &file, "id").is_empty(),
                            "fixture {file}: scenario id"
                        );
                        let frames = arr_key(scenario, &file, "frames");
                        assert!(!frames.is_empty(), "fixture {file}: no frames");
                        for frame in frames {
                            let frame = object(frame, &file, "frame");
                            assert!(
                                key(frame, &file, "directory").is_string(),
                                "fixture {file}: frame directory must be a string"
                            );
                            let payload = object(key(frame, &file, "payload"), &file, "payload");
                            assert!(
                                str_key(payload, &file, "id").parse::<u64>().is_ok(),
                                "fixture {file}: payload id is the ring sequence"
                            );
                            let kind = str_key(payload, &file, "type");
                            assert!(
                                declared.contains(&kind),
                                "fixture {file}: frame type {kind:?} is not declared by the union list"
                            );
                            assert!(
                                key(payload, &file, "properties").is_object(),
                                "fixture {file}: frame properties must be an object"
                            );
                        }
                    }
                }
                _unknown => {
                    let shape = match &raw {
                        Value::Object(o) => {
                            format!("object with keys {:?}", o.keys().collect::<Vec<_>>())
                        }
                        Value::Array(a) if !a.is_empty() => {
                            format!("array of {} elements", a.len())
                        }
                        other => format!("{other:?}"),
                    };
                    panic!(
                        "fixture {file}: new corpus file has no typed invariants (shape: {shape}); \
                         add per-file-type assertions to compat_fixtures_parse_with_minimal_invariants"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod upstream;
