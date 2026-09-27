//! `daemon::builder`: cohesive slice of the daemon construction.

use super::*;

/// The injectable EXTERNAL seams of the daemon's GitHub App surface — the
/// pieces of the `[cloud.github_app]` completion-SCM wiring that talk to the
/// network or read the wall clock, so a certification host can substitute a
/// deterministic double for any of them:
///
/// - [`Self::token_source`] replaces the installation-token credential
///   source (production builds the RS256 [`faktor_scm::GitHubAppTokenSource`]
///   over the operator-staged PKCS#8 key payload when `None`);
/// - [`Self::clock`] replaces the wall clock (`None` = the production
///   [`faktor_scm::SystemClock`]);
/// - [`Self::transport`] replaces the daemon's checked egress transport for
///   this surface only (`None` = the daemon's ONE policy-checked transport).
///
/// The API base, organization, app id and PR policy are CONFIG (the strict
/// `[cloud.github_app]` section) — never a seam. There is no config key,
/// environment variable or CLI flag for these seams; production entries
/// always pass [`GithubAppSeams::default`], so the daemon builder wires the
/// completion SCM provider from config by default.
#[derive(Clone, Default)]
pub struct GithubAppSeams {
    /// The installation-token source override.
    pub token_source: Option<Arc<dyn faktor_scm::InstallationTokenSource>>,
    /// The clock override.
    pub clock: Option<Arc<dyn faktor_scm::Clock>>,
    /// The egress transport override (a loopback-permitting fake-server
    /// transport in tests); production uses the daemon's checked transport.
    pub transport: Option<Arc<dyn HttpTransport>>,
}

/// Build the full daemon dependency graph (audit 12/17): every lifetime
/// authority is built EXACTLY ONCE in the order of
/// [`graph::DAEMON_CONSTRUCTION_ORDER`] — steps 1-2 (store/session + CAS)
/// run here, step 3 (the ONE supervisor) right after, and steps 4-16 are
/// inline in [`build_daemon_core`]. Serve/ACP/commands never construct a
/// supervisor, ledger, index or executor of their own — they take
/// references from the returned graph. The real filesystem stack is wired
/// in the core: workspace service, transactional edit engine, CAS-backed
/// checkpoints, sandbox policy engine, and the process supervisor.
pub fn build_daemon(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
) -> Result<DaemonGraph, String> {
    // Production entries pass the default (`None`) external seams: the
    // completion-SCM adapter is wired from `[cloud.github_app]` itself.
    build_daemon_with_scm_seams(data_dir, config, GithubAppSeams::default())
}

/// [`build_daemon`] with the explicit GitHub App EXTERNAL seams (transport
/// / token source / clock). The daemon's real `build_daemon_core` assembly
/// still transforms `[cloud.github_app]` into the completion SCM provider;
/// only the external I/O pieces are substituted, so the production-wiring
/// certification proves the CONFIG -> adapter wiring instead of
/// hand-installing the adapter after the build.
pub fn build_daemon_with_scm_seams(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
    scm_seams: GithubAppSeams,
) -> Result<DaemonGraph, String> {
    build_daemon_with_seams_and_planner(data_dir, config, scm_seams, None)
}

/// [`build_daemon`] with an explicit Faktor Acquire planner seam. Every
/// production entry ([`build_daemon`], the MCP/serve family) passes `None`,
/// so the ONE commerce service is constructed exactly as before with the
/// default [`faktor_commerce::service::AcquisitionPlanning`] implementation.
/// The production-wiring certification passes a spy here to prove the
/// daemon's REAL `build_daemon_core` assembly invokes
/// `AcquisitionPlanning::plan()` on the service's execution path. There is no
/// config key, environment variable or CLI flag for this seam.
pub fn build_daemon_with_acquisition_planner(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
    planner: Option<Arc<dyn faktor_commerce::service::AcquisitionPlanning>>,
) -> Result<DaemonGraph, String> {
    build_daemon_with_seams_and_planner(data_dir, config, GithubAppSeams::default(), planner)
}

/// The one shared sync construction path: steps 1-2 (store/session + CAS),
/// step 3 (the ONE supervisor) and the core (steps 4-21) over both explicit
/// seams (`None` in every production entry).
pub(crate) fn build_daemon_with_seams_and_planner(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
    scm_seams: GithubAppSeams,
    planner: Option<Arc<dyn faktor_commerce::service::AcquisitionPlanning>>,
) -> Result<DaemonGraph, String> {
    let config = config.unwrap_or_default();
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    // 1-2: the durable store + CAS (full integrity scan on this entry);
    // 3: ONE daemon supervisor, exactly as [`build_daemon`].
    let session = SessionManager::open(data_dir.join("store"), data_dir.join("cas"), true)
        .map_err(|e| e.to_string())?;
    let supervisor = ProcessSupervisor::new(session.cas());
    build_daemon_core(
        data_dir,
        session,
        supervisor,
        config,
        vec![],
        None,
        graph::SemanticCfg::default(),
        planner,
        scm_seams,
    )
}

/// The explicit-planner arm of the daemon's commerce construction: identical
/// validation, disabled parity, service config, connector registration and
/// artifact wiring to [`tools_market::open_commerce_service_with`], with the
/// caller's planner installed through
/// [`faktor_commerce::service::CommerceSourceService::open_with_planner`].
/// Production never reaches this function (`planner: None` at the step above).
pub(crate) fn open_commerce_service_with_planner(
    data_dir: &std::path::Path,
    cfg: &config::CommerceCfg,
    artifacts: Arc<dyn faktor_commerce::ArtifactStore>,
    seams: tools_market::CommerceSeams,
    planner: Arc<dyn faktor_commerce::service::AcquisitionPlanning>,
) -> Result<Option<Arc<faktor_commerce::service::CommerceSourceService>>, String> {
    cfg.validate()?;
    if !cfg.enabled {
        return Ok(None);
    }
    let service = faktor_commerce::service::CommerceSourceService::open_with_planner(
        data_dir,
        tools_market::service_config(cfg),
        artifacts,
        planner,
    )
    .map_err(|e| format!("commerce store: {e}"))?;
    let registration = tools_market::register_commerce_connectors(&service, cfg, &seams)?;
    tracing::info!(
        database = %data_dir
            .join(faktor_commerce::COMMERCE_DIR_NAME)
            .join(cfg.database_name())
            .display(),
        registered = registration.registered(),
        disabled = registration.disabled(),
        "commerce source enabled"
    );
    Ok(Some(service))
}

/// Async daemon build with the MCP layer (spec §31): configured servers are
/// spawned supervised BEFORE the agent is constructed so their dynamic
/// tools land in the registry next to the builtins (name collisions never
/// overwrite a builtin). A server that fails to connect is a loud warning,
/// not a daemon failure — the rest of the daemon still serves.
pub async fn build_daemon_with_mcp(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
) -> Result<DaemonGraph, String> {
    build_daemon_with_mcp_and_chunks(data_dir, config, None).await
}

pub async fn build_daemon_with_mcp_and_chunks(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
    chunk_tx: Option<std::sync::Arc<faktor_agent::ChunkSink>>,
) -> Result<DaemonGraph, String> {
    build_daemon_with_mcp_inner(
        data_dir,
        config,
        chunk_tx,
        false,
        graph::SemanticCfg::default(),
    )
    .await
}

/// Fast-start variant of [`build_daemon_with_mcp_and_chunks`]: the store is
/// opened with the bounded quick check (`SessionManager::open_quick`) instead
/// of the full integrity scan. `serve` — the production normal start — uses
/// this (audit 43); the deep scan lives under `doctor --deep` and crash
/// forensics. WAL recovery and migrations are NEVER skipped by the fast
/// path.
pub(crate) async fn build_daemon_with_mcp_and_chunks_fast(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
    chunk_tx: Option<std::sync::Arc<faktor_agent::ChunkSink>>,
    semantic: graph::SemanticCfg,
) -> Result<DaemonGraph, String> {
    build_daemon_with_mcp_inner(data_dir, config, chunk_tx, true, semantic).await
}

pub(crate) async fn build_daemon_with_mcp_inner(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
    chunk_tx: Option<std::sync::Arc<faktor_agent::ChunkSink>>,
    fast_open: bool,
    semantic: graph::SemanticCfg,
) -> Result<DaemonGraph, String> {
    let config = config.unwrap_or_default();
    let entries = config.mcp_servers()?;
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let session = if fast_open {
        SessionManager::open_quick(data_dir.join("store"), data_dir.join("cas"))
    } else {
        SessionManager::open(data_dir.join("store"), data_dir.join("cas"), true)
    }
    .map_err(|e| e.to_string())?;
    // ONE daemon supervisor (audit P0-40): the SAME Arc supervises every
    // MCP server child, every hook child, every tool/terminal child. The
    // servers are spawned first so the agent registry can see their tools.
    let supervisor = ProcessSupervisor::new(session.cas());
    let mut servers: Vec<Arc<faktor_mcp::McpServer>> = Vec::new();
    let mut mcp_tools: Vec<faktor_agent::Tool> = Vec::new();
    for entry in entries {
        let cfg = faktor_mcp::McpConfig {
            name: entry.name.clone(),
            command: entry.command,
            args: entry.args,
            env: vec![],
        };
        // Shared daemon supervisor: the McpServer holds the Arc so the
        // child lives for the daemon lifetime, in the SAME bounded
        // registry as hooks and terminals.
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            faktor_mcp::McpServer::connect(cfg, supervisor.clone()),
        )
        .await
        {
            Ok(Ok(server)) => {
                let name = server.name().to_string();
                match server.list_tools().await {
                    Ok(tools) => {
                        let n = tools.len();
                        for t in tools {
                            let tool = mcp_bridge::mcp_tool(server.clone(), &t);
                            mcp_tools.push(tool);
                        }
                        tracing::info!("mcp server {name}: {n} tool(s) wired");
                    }
                    Err(e) => {
                        tracing::warn!("mcp server {name}: tool listing failed: {e}");
                    }
                }
                servers.push(server);
            }
            Ok(Err(e)) => {
                tracing::warn!("mcp server {} failed to connect: {e}", entry.name);
            }
            Err(_) => {
                tracing::warn!("mcp server {} connect timed out after 10s", entry.name);
            }
        }
    }
    // Now build the core graph on the SAME store with the MCP tools (steps
    // 4-16 of the construction order; the servers already ride the ONE
    // supervisor above). Production external seams: the default ones.
    let mut graph = build_daemon_core(
        data_dir,
        session,
        supervisor,
        config,
        mcp_tools,
        chunk_tx,
        semantic,
        None,
        GithubAppSeams::default(),
    )?;
    graph.mcp_servers = servers;
    Ok(graph)
}

/// Maximum FAKTOR_HOOKS entries honored (bounding the env surface).
pub(crate) const MAX_ENV_HOOKS: usize = 8;

/// Parse the optional `FAKTOR_HOOKS` env into hook specs (pure fn, unit
/// tested). Format: semicolon-separated entries `event:command [args...]`;
/// the event is the snake_case `faktor_hooks::HookEvent` name (`pre_tool`,
/// `post_tool`, `task_complete`, …). Bounds: at most [`MAX_ENV_HOOKS`]
/// entries, ids are `env-N`. Every parsed spec runs with an env allowlist
/// (only `FAKTOR_HOOK_INPUT` passes through — use absolute command paths)
/// and the default FailClosed failure policy. Malformed entries (no colon,
/// unknown event, empty command) are warned about and skipped; a hostile
/// env can never panic or unboundedly grow the registry.
pub fn parse_hooks_env(raw: &str) -> Vec<faktor_hooks::HookSpec> {
    let mut out: Vec<faktor_hooks::HookSpec> = Vec::new();
    for entry in raw.split(';') {
        if out.len() >= MAX_ENV_HOOKS {
            tracing::warn!(
                "FAKTOR_HOOKS: at most {MAX_ENV_HOOKS} hooks are honored; ignoring the rest"
            );
            break;
        }
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (event_name, rest) = match entry.split_once(':') {
            Some(v) => v,
            None => {
                tracing::warn!(
                    "FAKTOR_HOOKS: skipping malformed entry {entry:?} (expected event:command)"
                );
                continue;
            }
        };
        let event: faktor_hooks::HookEvent = match serde_json::from_value(
            serde_json::Value::String(event_name.trim().to_string()),
        ) {
            Ok(e) => e,
            Err(_) => {
                tracing::warn!(
                    "FAKTOR_HOOKS: skipping entry {entry:?}: unknown event {event_name:?}"
                );
                continue;
            }
        };
        let mut parts = rest.split_whitespace();
        let command = match parts.next() {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => {
                tracing::warn!("FAKTOR_HOOKS: skipping entry {entry:?}: empty command");
                continue;
            }
        };
        let args: Vec<String> = parts.map(str::to_string).collect();
        out.push(faktor_hooks::HookSpec {
            id: format!("env-{}", out.len()),
            events: vec![event],
            command,
            args,
            env_allowlist: true,
            ..Default::default()
        });
    }
    out
}

/// The capability envelope the daemon grants its hook registry: the full
/// lattice (the operator-configured FAKTOR_HOOKS commands are as trusted
/// as the daemon env that named them — the envelope check refuses only
/// scopes a future config surface tries to grant beyond this).
pub(crate) const DAEMON_HOOK_ENVELOPE: CapabilitySet = CapabilitySet::ALL;

/// Build an optional lifecycle-hook registry over the DAEMON supervisor
/// (audit P0-40). Each parsed spec is logged; a spec the registry rejects
/// is a loud warning, never a daemon failure.
pub(crate) fn env_hook_registry(
    supervisor: &Arc<ProcessSupervisor>,
) -> Option<Arc<faktor_hooks::HookRegistry>> {
    let specs = parse_hooks_env(&std::env::var("FAKTOR_HOOKS").unwrap_or_default());
    hook_registry(supervisor, specs)
}

/// Shared hook-registry construction (test seam + env path): the registry
/// is rooted at the GIVEN supervisor and granted the daemon envelope, so
/// every hook child lands in the daemon's single bounded registry.
pub(crate) fn hook_registry(
    supervisor: &Arc<ProcessSupervisor>,
    specs: Vec<faktor_hooks::HookSpec>,
) -> Option<Arc<faktor_hooks::HookRegistry>> {
    if specs.is_empty() {
        return None;
    }
    let registry = Arc::new(faktor_hooks::HookRegistry::with_supervisor(
        supervisor.clone(),
        DAEMON_HOOK_ENVELOPE,
    ));
    for spec in specs {
        tracing::info!(
            "hook {}: {:?} -> {} {}",
            spec.id,
            spec.events,
            spec.command,
            spec.args.join(" ")
        );
        if let Err(e) = registry.register(spec) {
            tracing::warn!("FAKTOR_HOOKS: hook rejected: {e}");
        }
    }
    Some(registry)
}

/// Durable workspace roots for the instruction resolver (P0-32 + P0-48):
/// every resolution goes through the daemon's SessionManager workspace
/// table — the process CWD and any static config default root are NEVER
/// consulted. A workspace row without a root (or an unknown workspace id)
/// resolves to `None`, which the resolver turns into the documented Empty
/// instruction set. While EXACTLY ONE session of the workspace carries a
/// live shadow row (a shadowed single-agent drive), the workspace's rules
/// resolve from that shadow root (P0 isolation: mutating runs always carry
/// one): the drive reads the instruction environment of the world it
/// mutates. Ambiguity (more
/// than one live shadow — hostile residue the executor discipline never
/// produces) degrades loudly to the stored root, never a guess.
pub(crate) struct SessionWorkspaceRoots(pub(crate) Arc<SessionManager>);

impl SessionWorkspaceRoots {
    pub(crate) fn resolve(&self, workspace_id: u64) -> Option<PathBuf> {
        if workspace_id == 0 {
            return None;
        }
        let ws = faktor_core::id::WorkspaceId::new(workspace_id);
        match self.0.live_workspace_shadow_root(ws) {
            Ok(Some(shadow)) => {
                tracing::debug!(
                    workspace = %workspace_id, shadow = %shadow.display(),
                    "workspace instructions resolve from the live shadow root (shadow mutation drive)"
                );
                Some(shadow)
            }
            Ok(None) => match self.0.workspace_root(ws) {
                Ok(Some(root)) => Some(root),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(error = %e, workspace = %workspace_id,
                        "durable workspace-root lookup failed; resolving no instructions for this session");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, workspace = %workspace_id,
                    "ambiguous live shadows on this workspace; resolving instructions from the stored workspace root");
                match self.0.workspace_root(ws) {
                    Ok(Some(root)) => Some(root),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, workspace = %workspace_id,
                            "durable workspace-root lookup failed; resolving no instructions for this session");
                        None
                    }
                }
            }
        }
    }
}

impl faktor_instructions::WorkspaceRootProvider for SessionWorkspaceRoots {
    fn workspace_root(&self, workspace_id: u64) -> Option<PathBuf> {
        self.resolve(workspace_id)
    }
}

/// The daemon's per-workspace instruction resolver (P0-32): ONE resolver
/// over the real SessionManager, shared by every AgentDeps. Roots are
/// resolved per session from the durable workspace table at resolve time —
/// there is no single default root at daemon build (audit 31's per-session
/// wiring, now implemented instead of a None loader).
pub(crate) fn daemon_instructions_resolver(
    session: &Arc<SessionManager>,
) -> Arc<faktor_instructions::InstructionResolver> {
    Arc::new(faktor_instructions::InstructionResolver::new(
        Arc::new(SessionWorkspaceRoots(session.clone())),
        faktor_instructions::DEFAULT_RESOLVER_CACHE_ENTRIES,
    ))
}

/// The outbound secret-scan config of the daemon's transports: every
/// request body is whole-payload scanned with a [`SecretRegistry`] fed from
/// the CONFIGURED provider keys AND every configured commerce connector
/// credential (resolved from its configured env-var NAME; the name itself
/// is never a secret and nothing is logged). The registry fingerprints
/// values only — its Debug stays redacted (counts only).
pub(crate) fn daemon_outbound_scan(config: &config::Config) -> OutboundScanConfig {
    let mut registry = SecretRegistry::new();
    for p in &config.providers {
        if let Some(key) = p.key() {
            registry.register(key.expose().as_bytes());
        }
    }
    if config.commerce.enabled {
        for (_, credential) in config.commerce.connectors.enabled() {
            register_commerce_credential(&mut registry, credential);
        }
    }
    OutboundScanConfig {
        registry: Some(Arc::new(registry)),
        ..Default::default()
    }
}

/// Register one configured commerce credential VALUE with the outbound
/// scan registry. The config carries env-var names only; an unset/empty/
/// non-unicode variable is left unregistered exactly like a provider key
/// whose env var is unset (the connector itself registers Disabled). The
/// value is never logged, named or rendered.
pub(crate) fn register_commerce_credential(
    registry: &mut SecretRegistry,
    credential: config::ConnectorCredential<'_>,
) {
    let mut register_env = |name: &str| {
        if let Ok(value) = std::env::var(name) {
            if !value.trim().is_empty() {
                registry.register(value.as_bytes());
            }
        }
    };
    match credential {
        config::ConnectorCredential::ApiKey(Some(name)) => register_env(name),
        config::ConnectorCredential::Marketplace { api: Some(api), .. } => {
            // Only the secret-bearing names are registered with the scan
            // registry; the expiry name holds a timestamp, not a secret.
            for name in [api.app_key_env, api.app_secret_env, api.access_token_env]
                .into_iter()
                .flatten()
            {
                register_env(name);
            }
        }
        config::ConnectorCredential::OAuthPair(Some(client_id), Some(client_secret)) => {
            register_env(client_id);
            register_env(client_secret);
        }
        _ => {}
    }
}

/// The ONE egress transport every configured adapter executes through:
/// policy-checked with the daemon's SandboxPolicy network gate installed
/// (default-deny on any destination the allowlist does not match, BEFORE a
/// connect) and the outbound whole-payload secret scan attached.
///
/// The destination policy is REQUIRED: a sandbox gate that installed no
/// allowlist fails closed to the EMPTY policy (deny everything) instead of
/// silently default-allowing every destination. `addresses` carries the
/// explicit address-class rule — external-only by default; a provider entry
/// whose typed config says `allow_loopback` gets the one explicit loopback
/// exception, and nothing else.
pub(crate) fn daemon_egress_transport(
    policy: &faktor_sandbox::SandboxPolicy,
    scan: OutboundScanConfig,
    addresses: EgressAddressPolicy,
) -> Result<Arc<dyn HttpTransport>, String> {
    let destinations = policy
        .network
        .installed()
        .cloned()
        .unwrap_or_else(faktor_security::destination::DestinationPolicy::empty);
    // Construction is FALLIBLE: a client-build failure is a typed start-up
    // refusal, never a silently degraded (fallback) client.
    let transport = PolicyCheckedHttpTransport::try_with_policy_scan_and_addresses(
        destinations,
        Some(scan),
        addresses,
    )
    .map_err(|e| format!("egress client construction: {e}"))?;
    Ok(Arc::new(transport))
}

/// The daemon verification service from the configured `[verification]`
/// section: sane values build the typed service under the section's
/// policy; `quick_max_s: 0` yields the DISABLED service (fail closed —
/// mutating turns classify Unverified, never silently complete).
///
/// The executor rides THE daemon supervisor (audit P0-5/P0-6 process
/// consolidation): verification shares the single process runtime — its
/// live-child ceiling, its capture ring and its whole-tree kill paths —
/// instead of spawning a second process layer. The graph core calls this
/// at step 12 of the construction order with a FIELD borrow of the config
/// (the provider loop has consumed the rest of the config by then).
pub(crate) fn daemon_verification(
    section: &config::VerificationCfg,
    supervisor: &Arc<ProcessSupervisor>,
) -> Arc<faktor_agent::VerificationService> {
    match section.policy() {
        Some(policy) => faktor_agent::VerificationService::new(
            Arc::new(faktor_verify::exec::AsyncCheckExecutor::from_supervisor(
                supervisor.clone(),
            )),
            policy,
        ),
        None => faktor_agent::VerificationService::disabled(),
    }
}

/// Map the parsed `[efficiency]` section onto the agent's flag type
/// (additive; every flag defaults `false`). The runtime applies
/// `failure_learning` to the context prior; the remaining flags are carried
/// by `AgentDeps` for their efficiency components.
pub(crate) fn efficiency_flags(cfg: &config::EfficiencyCfg) -> faktor_agent::EfficiencyFlags {
    faktor_agent::EfficiencyFlags {
        failure_learning: cfg.failure_learning,
        ccr: cfg.ccr,
        typed_handoff: cfg.typed_handoff,
        semantic_context: cfg.semantic_context,
        rework_routing: cfg.rework_routing,
    }
}

/// The production failure-learning prior adapter (audit 68, closing audits
/// 65-69/82): the planner's
/// [`faktor_context::information::FailurePrior`] over the DURABLE learning
/// corpora of the daemon's session manager, snapshotted as a key -> risk
/// index so a per-candidate lookup is one map probe with no allocation.
///
/// The index is built through
/// [`faktor_learning::SessionLearningStore`] over the sessions' typed
/// `learning_record` ledger rows: no learning state lives only in memory,
/// and a daemon reopen re-reads the same rows (reopen-safe). The cached
/// index is refreshed when the per-session ledger stamp advances, so a
/// learning mined DURING this daemon's lifetime protects its matching
/// evidence candidate on the next plan — the production loop closes without
/// a restart.
///
/// Lookup is keyed by [`faktor_context::ContextCandidate::omission_keys`]
/// — the learning identities the wire planner surfaces from
/// `learning:<digest>` evidence paths — never by the candidate's render id.
///
/// Hostile-value contract: every risk is clamped to `[1, 2]` by
/// `omission_risk_of` and is always finite, so this adapter can only ever
/// PROTECT a candidate up to 2x. Panics are NOT caught by the runtime (the
/// planner consults this daemon-global handle in-process), so this adapter
/// is total: a poisoned mutex is recovered, hostile keys are ignored, and a
/// corrupt ledger row is logged loudly while that session's corpus stays
/// neutral — never a failed turn.
pub(crate) struct LearningRiskPrior {
    pub(crate) session: Arc<SessionManager>,
    pub(crate) cache: std::sync::Mutex<RiskCache>,
}

/// The cached merged omission-risk index plus the durable stamp it was built
/// from: `(session id, newest ledger seq)` sorted by session id. The stamp
/// changes exactly when a session's typed ledger advances — the only way a
/// learning corpus can grow.
#[derive(Debug, Default)]
pub(crate) struct RiskCache {
    pub(crate) stamp: Vec<(u64, i64)>,
    pub(crate) risks: std::collections::HashMap<faktor_core::FileHash, f64>,
}

impl LearningRiskPrior {
    /// Build the adapter over the daemon's session manager, snapshotting
    /// every durable corpus ONCE (reopen-safe). An empty corpus yields an
    /// empty index, so every lookup is neutral (`1.0`) and context selection
    /// stays byte-identical to the flag-off path.
    pub(crate) fn from_session(session: Arc<SessionManager>) -> Self {
        let stamp = Self::stamp(&session);
        let risks = Self::merged_risks(&session);
        tracing::info!(
            learnings = risks.len(),
            "failure_learning enabled: durable learning corpora read from the session manager (empty corpus => neutral risk 1.0)"
        );
        Self {
            session,
            cache: std::sync::Mutex::new(RiskCache { stamp, risks }),
        }
    }

    /// Cheap durable stamp of every session's typed ledger (one session-list
    /// query plus one indexed MAX per session). A failed read contributes a
    /// `-1` sentinel so the next successful read still differs and forces a
    /// rebuild instead of silently trusting a stale corpus.
    pub(crate) fn stamp(session: &Arc<SessionManager>) -> Vec<(u64, i64)> {
        let store = session.store();
        let rows = match store.list_sessions(None) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(%error, "learning prior: session list read failed; corpus stamp is unknown");
                return Vec::new();
            }
        };
        let mut stamp: Vec<(u64, i64)> = rows
            .iter()
            .map(|row| (row.id.raw(), store.ledger_max_seq(row.id).unwrap_or(-1)))
            .collect();
        stamp.sort_unstable();
        stamp
    }

    /// Merge every session's durable corpus index (keyed by pattern AND
    /// failure digest, max risk per key). A corrupt/unreadable corpus is
    /// LOUD but non-fatal: that session contributes nothing (neutral), the
    /// rest of the daemon keeps serving, and the next stamp check retries.
    pub(crate) fn merged_risks(
        session: &Arc<SessionManager>,
    ) -> std::collections::HashMap<faktor_core::FileHash, f64> {
        let mut risks = std::collections::HashMap::new();
        let rows = match session.list_sessions(None) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(%error, "learning prior: session list read failed; every corpus stays neutral");
                return risks;
            }
        };
        for handle in rows {
            let corpus = match faktor_learning::SessionLearningStore::open(
                handle.clone(),
                faktor_learning::DEFAULT_MEMORY_CAPACITY,
            ) {
                Ok(corpus) => corpus,
                Err(error) => {
                    tracing::error!(
                        session = %handle.id(),
                        %error,
                        "learning prior: corrupt/unreadable learning corpus; this session stays neutral"
                    );
                    continue;
                }
            };
            for learning in corpus.all() {
                let risk = faktor_learning::omission_risk_of(learning.confidence_ppm);
                for key in [learning.pattern_digest(), learning.pattern.failure.digest()] {
                    risks
                        .entry(key)
                        .and_modify(|existing: &mut f64| *existing = existing.max(risk))
                        .or_insert(risk);
                }
            }
        }
        risks
    }

    /// The current cache, rebuilt from the durable ledger rows when the
    /// stamp advanced.
    pub(crate) fn cache(&self) -> std::sync::MutexGuard<'_, RiskCache> {
        let mut cache = match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let stamp = Self::stamp(&self.session);
        if stamp != cache.stamp {
            let risks = Self::merged_risks(&self.session);
            tracing::info!(
                learnings = risks.len(),
                "learning prior: corpus stamp advanced; durable learning index re-read"
            );
            cache.stamp = stamp;
            cache.risks = risks;
        }
        cache
    }
}

impl faktor_context::information::FailurePrior for LearningRiskPrior {
    fn omission_risk(&self, candidate: &faktor_context::ContextCandidate) -> f64 {
        // Non-learning candidates expose no keys: neutral without touching
        // the durable stamp (the overwhelmingly common case).
        if candidate.omission_keys.is_empty() {
            return faktor_learning::OMISSION_RISK_NEUTRAL;
        }
        let cache = self.cache();
        faktor_learning::omission_risk_for_keys(&cache.risks, &candidate.omission_keys)
    }
}

/// Build the daemon's failure-learning prior handle (audit 68): `Some` ONLY
/// when `[efficiency] failure_learning` is on; `None` otherwise (the
/// runtime then takes the byte-identical baseline planner path). `Some`
/// always carries the durable corpus over the daemon's session manager.
pub(crate) fn daemon_context_prior(
    enabled: bool,
    session: &Arc<SessionManager>,
) -> Option<Arc<dyn faktor_context::information::FailurePrior + Send + Sync>> {
    if !enabled {
        return None;
    }
    Some(Arc::new(LearningRiskPrior::from_session(session.clone())))
}

/// The graph construction core (audit 12/17): steps 4-21 of
/// [`graph::DAEMON_CONSTRUCTION_ORDER`] are built HERE, inline and in the
/// documented order (the ordering test scans this function's body).
/// Callers open the store and create the ONE supervisor (steps 1-3) — the
/// async MCP connect must ride that same supervisor — and everything else
/// of the daemon lifetime is this function's construction. The `scm_seams`
/// are EXTERNAL seam substitutions only (`None` in production): the
/// completion SCM provider is transformed from `[cloud.github_app]` here,
/// never installed onto the executor after the build.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_daemon_core(
    data_dir: &std::path::Path,
    session: Arc<SessionManager>,
    supervisor: Arc<ProcessSupervisor>,
    config: config::Config,
    extra_tools: Vec<faktor_agent::Tool>,
    chunk_tx: Option<std::sync::Arc<faktor_agent::ChunkSink>>,
    semantic: graph::SemanticCfg,
    planner: Option<Arc<dyn faktor_commerce::service::AcquisitionPlanning>>,
    scm_seams: GithubAppSeams,
) -> Result<DaemonGraph, String> {
    // Step 4 — checked transport/security: the daemon's ONE sandbox policy
    // from the `[sandbox]` section (destination gate + OS-level
    // network-isolation guarantee), the outbound whole-payload secret scan
    // (P0-36/37/38; keys registered without logging — Debug stays redacted),
    // and the ONE policy-checked + secret-scanned egress transport every
    // configured adapter executes through. No adapter is ever constructed
    // with a permissive default transport.
    let sandbox_policy = config
        .sandbox_policy()
        .map_err(|e| format!("sandbox config: {e}"))?;
    let egress = daemon_outbound_scan(&config);
    // The scan config is shared: the provider transport and the commerce
    // checked transport each install their own destination policy over the
    // SAME configured-secret registry (provider keys + commerce credentials).
    let transport = daemon_egress_transport(
        &sandbox_policy,
        egress.clone(),
        EgressAddressPolicy::EXTERNAL,
    )?;
    // A second client object with the ONE explicit loopback exception,
    // built ONLY for entries whose typed config asked for it
    // (`allow_loopback`): every other entry keeps the external-only
    // transport, and no address class is ever allowed globally.
    let loopback_transport =
        daemon_egress_transport(&sandbox_policy, egress.clone(), EgressAddressPolicy::LOCAL)?;
    // Steps 5-6 — provider registry + catalog/pricing: every configured
    // adapter is built through the checked transport its entry selected;
    // Ollama providers are kept CONCRETE for live probing (spec §10:
    // warm-up must reach the instance the registry serves). Catalog/pricing
    // rows (built-in tables + configured overrides/ceilings) ride the
    // adapter constructions and the registry's catalog rows.
    let mut providers = ProviderRegistry::new();
    let mut ollama_warmers: Vec<Arc<faktor_ollama::OllamaProvider>> = Vec::new();
    for p in &config.providers {
        let provider_transport = if p.allows_loopback() {
            loopback_transport.clone()
        } else {
            transport.clone()
        };
        if let Some(ollama) = p.build_ollama(provider_transport.clone()) {
            // The registered instance carries this entry's catalog
            // authorities (strict billing origin + pricing overrides + the
            // `quality` declaration, item 1) while the CONCRETE Arc stays
            // for live probing.
            let dyn_arc: Arc<dyn Provider> = ollama.clone();
            match p.wrap_catalog_authority(dyn_arc) {
                Ok(wrapped) => {
                    providers
                        .try_register(wrapped)
                        .map_err(|e| format!("provider {} failed to register: {e}", p.id()))?;
                    ollama_warmers.push(ollama);
                }
                Err(e) => tracing::warn!("provider {} failed to build: {e}", p.id()),
            }
            continue;
        }
        match p.build(provider_transport) {
            Ok(provider) => providers
                .try_register(provider)
                .map_err(|e| format!("provider {} failed to register: {e}", p.id()))?,
            Err(e) => tracing::warn!("provider {} failed to build: {e}", p.id()),
        }
    }
    let providers = Arc::new(providers);
    let store = session.store();
    let cas = session.cas();
    // Step 7 — economic routing + the durable verified-outcome registry
    // (P0-2/6/12): the routing policy is built from the REGISTERED
    // providers + the config's mode (Economy default; a Pinned mode that
    // names an unregistered provider/model refuses the daemon at boot —
    // never a silent Economy). The outcome registry rides this store
    // (audit items 13/14/L): verified samples the runtime records at the
    // deterministic gate sites land here and every later route consult
    // reads them back — routing and the recorded outcome history share
    // one store-backed registry, exactly like the pricing authority.
    let routing_mode = config
        .routing_mode
        .clone()
        .unwrap_or(faktor_core::model::RoutingMode::Economy);
    let routing = graph::economic_routing_policy_with_outcomes(
        &providers,
        routing_mode,
        Arc::new(faktor_agent::StoreOutcomeStore::new(store.clone())),
    )
    .map_err(|e| format!("routing config error: {e}"))?;
    // Step 8 — the durable cost ledger over THIS daemon's store (P0-6/12):
    // one reservation per paid model call, settled exactly once. Crash
    // recovery abandons every OPEN reservation of a previous process BEFORE
    // the first turn (never counted as spent).
    let budgets = faktor_session::DurableBudgetLedger::new(session.clone());
    budgets.recover_after_restart();
    // Step 9 — the repository IndexService (audits 30/64) over the SAME
    // store + workspace service + data root the runtime's evidence ladder
    // hosts; the graph pre-hosts the durable index authority at boot. A
    // hostile/unwritable data root degrades exactly like the runtime's own
    // lazy host: warn + None — the daemon keeps serving on the bounded
    // evidence scan, never a broken first prompt.
    let workspaces = faktor_fs::WorkspaceFileService::new();
    let index_data_root = store
        .path()
        .parent()
        .map(|p| p.join("index_data"))
        .unwrap_or_else(|| std::path::PathBuf::from("index_data"));
    let index = match faktor_index::IndexService::open_with_supervisor(
        store.clone(),
        index_data_root,
        workspaces.clone(),
        supervisor.clone(),
    ) {
        Ok(svc) => {
            tracing::info!("repository IndexService hosted");
            Some(svc)
        }
        Err(e) => {
            tracing::warn!("repository IndexService unavailable: {e}");
            None
        }
    };
    // Step 10 — evidence/cold: the daemon's evidence provider (spec §20):
    // the bounded per-workspace scan + search every session's context
    // engine consults while the index has no Ready generation. The
    // configured `[embeddings]` selection (model + provider + policy) is
    // resolved HERE against the registry built above and rides the provider
    // into BOTH evidence paths (the cold scan and the agent's index-backed
    // assembly): an absent/unresolvable selection under `best_effort`
    // degrades to lexical/symbol-only retrieval, never a fabricated vector.
    let embedder = config
        .semantic_embedder(&providers, &faktor_core::retry::RetryPolicy::default())
        .map_err(|e| format!("embeddings config: {e}"))?;
    let repo_evidence = Arc::new(RepoEvidence::new(session.clone(), embedder));
    // Step 11 — per-workspace repository instructions (P0-32): the
    // resolver is built over the daemon's SessionManager workspace table
    // ONCE — every later resolution reads a session's DURABLE workspace
    // root, never a process CWD and never a static config default root.
    // Sessions whose workspace carries no root resolve to an Empty set.
    let instructions_resolver = daemon_instructions_resolver(&session);
    // Step 12 — the typed verification engine (P0-9/10 migration): REQUIRED
    // checks the agent derives from its OWN file changes execute as
    // (program, argv) specs through the async executor ON THE DAEMON
    // SUPERVISOR (audit P0-5/P0-6) — never `sh -c`, never a second process
    // runtime. Budgets come from the configured [verification] section
    // (defaults: quick <= 60 s, unit <= 600 s inline, full = durable
    // background verification jobs; quick_max_s 0 = disabled + fail closed).
    let verification = daemon_verification(&config.verification, &supervisor);
    // Steps 13-16 — semantic + learning + memory + tokenizers: the ONE
    // semantic-provider registry built from the strict `[semantic]` section
    // over THIS daemon's supervisor + checked transport (the SAME Arc flows
    // to the agent and the server introspection surface); the durable
    // failure-learning prior handle built once over this daemon's session
    // store; the project-memory authority over the SAME store; and the
    // tokenizer registry with the real local backends (unregistered
    // identities keep their conservative UpperBound label).
    let semantic = graph::semantic_registry(&semantic, &supervisor, &transport)?;
    let learning = daemon_context_prior(config.efficiency.failure_learning, &session);
    let memory = graph::DaemonMemory::new(store.clone());
    let tokenizers = Arc::new(faktor_context::TokenizerRegistry::with_builtin_backends());
    // Tokenizer compatibility certification (audit item 22): every
    // built-in and configured catalog row is classified
    // `exact | upper_bound | unsupported` against the ONE tokenizer
    // registry; an exact-accounting row without an exact tokenizer refuses
    // the daemon configuration immediately (never a silent estimate), and
    // conservative-accounting rows stay routable with their UpperBound
    // uncertainty recorded on the certification row.
    let tokenizer_rows = graph::certify_provider_tokenizers(&providers, &tokenizers)?;
    for row in &tokenizer_rows {
        if row.is_exact() {
            tracing::debug!(
                provider = %row.provider,
                model = %row.model,
                tokenizer = %row.tokenizer,
                compatibility = row.compatibility.as_str(),
                estimate_kind = row.estimate_kind,
                "tokenizer certification"
            );
        } else {
            // Conservative-accounting row: routable, but every count it
            // produces is an UpperBound — the uncertainty is attached to
            // the daemon's budget/routing telemetry explicitly (never
            // silently treated as exact).
            tracing::info!(
                provider = %row.provider,
                model = %row.model,
                tokenizer = %row.tokenizer,
                compatibility = row.compatibility.as_str(),
                estimate_kind = row.estimate_kind,
                uncertainty = row.uncertainty,
                "token accounting uncertainty: conservative UpperBound estimates"
            );
        }
    }
    // Step 17 — Faktor Acquire (docs/acquire.md §13/§14): the ONE commerce
    // source service, constructed AFTER memory/tokenizers and BEFORE the
    // agent, because the `source_market` tool needs its Arc before the final
    // ToolRegistry is injected. The enabled site adapters are registered
    // through the bridge at construction time (transport = the COMMERCE
    // checked egress over the parsed per-source destination policy, browser
    // authority only while `[commerce.browser]` is enabled, credentials by
    // env-var NAME, outbound whole-payload scan over the SAME configured
    // credential values) — exactly once, before the tool registry is
    // finalized. Disabled (the default) constructs `None`: no commerce
    // directory, database, connector runtime, browser state or network.
    let commerce_seams =
        tools_market::commerce_seams(&config.commerce, data_dir, egress.clone(), &supervisor)?;
    // The shared registered-value guard of the commerce surface: the
    // connectors fill it at registration and the tool gateway + CAS artifact
    // store scrub through the SAME Arc.
    let commerce_secrets = commerce_seams.secrets.clone();
    let commerce_artifacts = Arc::new(tools_market::CasArtifacts::with_secrets(
        cas.clone(),
        commerce_secrets.clone(),
    ));
    // Production (`planner: None`) constructs through the tools_market entry
    // exactly as before; the `Some` arm exists only for the
    // production-wiring certification, which proves the planner's `plan()`
    // runs on this very daemon assembly path.
    let commerce = match planner {
        None => tools_market::open_commerce_service_with(
            data_dir,
            &config.commerce,
            commerce_artifacts,
            commerce_seams,
        )?,
        Some(planner) => open_commerce_service_with_planner(
            data_dir,
            &config.commerce,
            commerce_artifacts,
            commerce_seams,
            planner,
        )?,
    };
    // The builtin tool registry + the MCP tools (a collision never replaces
    // a builtin) and the engine layer the runtime hands its tools: edit
    // engine, CAS-backed checkpoints, the permission engine over the
    // daemon's sandbox policy, the permission channel, and the lifecycle
    // hooks rooted at the daemon's ONE supervisor.
    let mut tools = ToolRegistry::new();
    tools.register(tools::read_file_tool());
    tools.register(tools::write_file_tool());
    tools.register(tools::edit_file_tool());
    tools.register(tools::search_tool());
    tools.register(tools::run_command_tool());
    // Coordination board: the tools are agent-crate definitions, the board
    // authority is THIS daemon's session manager (run-family scoped).
    let board_gateway = Arc::new(tools::SessionBoardGateway::new(session.clone()));
    tools.register(faktor_agent::board_post_tool(board_gateway.clone()));
    tools.register(faktor_agent::board_read_tool(board_gateway));
    // Faktor Acquire (docs/acquire.md §4): the `source_market` tool enters
    // the registry LAZILY with the deterministic activation policy and the
    // §4 phase mask — until a signal/product URL/`/source on` activates it,
    // it contributes zero schema bytes and zero schema tokens.
    if let Some(commerce) = &commerce {
        tools.register_lazy(
            tools_market::source_market_tool_with_secrets(
                commerce.clone(),
                Some(commerce_secrets.clone()),
            ),
            tools_market::source_market_exposure(),
        );
    }
    for t in extra_tools {
        if tools.names().contains(&t.name) {
            tracing::warn!(
                "mcp tool {} collides with a builtin; the builtin wins",
                t.name
            );
            continue;
        }
        tools.register(t);
    }
    let edit = Arc::new(faktor_edit::EditEngine::new(workspaces.clone()));
    let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(cas.clone(), store));
    let sandbox = Arc::new(faktor_sandbox::PermissionEngine::new(sandbox_policy, None));
    let permissions = ChannelPermissionRequester::new(std::time::Duration::from_secs(300));
    let hooks = env_hook_registry(&supervisor);
    // Step 13 — the AgentRuntime over every authority above: the routing
    // policy, the budget ledger, the evidence provider, the instruction
    // resolver, the verification service and the ONE supervisor all enter
    // the runtime through this single deps literal.
    let agent = AgentRuntime::new(AgentDeps {
        session: session.clone(),
        providers: providers.clone(),
        chunk_sink: chunk_tx,
        permission_requester: permissions.clone(),
        evidence: repo_evidence.clone(),
        tools: Arc::new(tools),
        cas: Some(cas),
        workspaces,
        edit: Some(edit),
        snapshots: Some(snapshots),
        sandbox: Some(sandbox),
        supervisor: Some(supervisor.clone()),
        verification: verification.clone(),
        hooks,
        instructions_resolver: instructions_resolver.clone(),
        routing: routing.clone(),
        budgets: budgets.clone(),
        model: config.model.clone(),
        compaction_model: config.compaction_model,
        compact_at_usage: config.compact_at_usage,
        instructions: config.instructions,
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::NativeWithRepair,
        tool_deadline_ms: 30_000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: semantic.clone(),
        // Audit 68: the parsed `failure_learning` flag decides whether a
        // prior handle is installed (and the runtime then applies it);
        // `false` installs `None` and the whole `[efficiency]` section
        // rides the additive default. The handle was built ONCE above and
        // the graph holds the SAME Arc.
        context_prior: learning.clone(),
        efficiency: efficiency_flags(&config.efficiency),
    })
    .map_err(|e| e.to_string())?;
    for ollama in ollama_warmers {
        warm_ollama(ollama);
    }
    // Steps 14-16 — the orchestration authorities (audits P0-20/21/23/61,
    // P0-48 + P0 isolation): OrchestratorRuntime + ShadowRoots + the ONE
    // TaskExecutor over the SAME orchestrator. The executor ALWAYS carries
    // the shadow service — the production constructor requires it — so every
    // mutating run executes in an isolated candidate; no config key, DTO
    // field or mode value can disable isolation.
    let orchestrator =
        faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
    // Shadow mutation roots: rooted at `<data dir>/shadows`. reconcile()
    // at boot handles crash residue; the service's Drop removes every
    // shadow on graceful daemon shutdown.
    let shadows_root = data_dir.join(faktor_orchestrator::runtime::shadow::SHADOWS_DIR_NAME);
    let shadows =
        faktor_orchestrator::runtime::shadow::ShadowRoots::new(session.clone(), shadows_root)
            .map_err(|e| e.to_string())?;
    if let Err(e) = shadows.reconcile() {
        tracing::warn!(error = %e, "shadow reconcile after daemon start");
    }
    let tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
        &orchestrator,
        session.clone(),
        agent.clone(),
        shadows.clone(),
    );
    // P2 completion-step execution: the strict `[completion]` section feeds
    // the executor's commit/push/PR policy. An absent section keeps the
    // inert defaults (push origin, base main, unconfigured PR => Skipped);
    // an invalid section fails daemon startup instead of half-running.
    tasks
        .configure_completion_steps(
            config
                .completion
                .steps_config()
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    // Step 21 (continued) — the daemon's ONE durable SCM store and the
    // completion-step SCM provider: an enabled `[cloud]` section opens the
    // `scm.db` store HERE (the same allocation serve hands to the native
    // control-plane/SCM surface — no second store authority), and an enabled
    // `[cloud.github_app]` section is TRANSFORMED into the canonical
    // `faktor_scm::GitHubCompletionScm` adapter of THIS executor at
    // construction time. Disabled sections construct nothing (fail-closed
    // parity: a contracted PR step then records the explicit
    // `native_pr_scm_not_configured` blocker). Only the token source and
    // clock may arrive through the external seams; API base, app id and
    // organization are config.
    let scm_store: Option<Arc<dyn faktor_scm::ScmStore>> = if config.cloud.enabled {
        let path = config
            .cloud
            .scm_path(data_dir)
            .map_err(|e| format!("cloud config: {e}"))?;
        let path = enabled_section_db_path("cloud scm", path)?;
        Some(Arc::new(faktor_scm::SqliteScmStore::open(&path).map_err(
            |e| format!("cloud scm store {}: {e}", path.display()),
        )?))
    } else {
        None
    };
    wire_completion_scm(
        &config.cloud,
        data_dir,
        &transport,
        scm_store.clone(),
        &scm_seams,
        &tasks,
    )?;
    // THE durable evidence authority is the runtime's own allocation: the
    // graph stores the same `Arc` the runtime's compiler/archiver use, and
    // serve hands the same `Arc` to the native server. No second authority
    // is constructed, so ids/scope/backing can never disagree between the
    // runtime and the server.
    let evidence = agent.evidence_authority().clone();
    Ok(DaemonGraph {
        session,
        supervisor,
        transport,
        providers,
        permissions,
        mcp_servers: vec![],
        routing,
        budgets,
        index,
        repo_evidence,
        instructions: instructions_resolver,
        verification,
        semantic,
        learning,
        memory,
        tokenizers,
        commerce,
        scm: scm_store,
        agent,
        evidence,
        orchestrator,
        shadows,
        tasks,
    })
}

use faktor_learning::LearningStore as _;
